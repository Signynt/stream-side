// src/backend/desktop.rs
//
// Десктопный бекенд: HEVC-декодер через ffmpeg-next + VAAPI hardware decode.
//
// Поток данных:
//   VideoPacket.payload (HEVC NAL)
//     → avcodec (VAAPI hw decode)
//     → AVFrame(VAAPI surface)
//     → av_hwframe_transfer_data → AVFrame(NV12, CPU)  [reused each frame]
//     → copy into pooled Vec<u8>                       [no malloc at steady state]
//     → YuvFrame → WgpuState → экран
//
// Ключевые оптимизации по сравнению с наивной реализацией:
//
// | Было                          | Стало                                   |
// |-------------------------------|-----------------------------------------|
// | av_frame_alloc() каждый кадр  | один *mut AVFrame, av_frame_unref()     |
// | Video::new(NV12) каждый кадр  | один Video, переиспользуется scaler'ом  |
// | .to_vec() × 2                 | copy_from_slice в пул + mem::replace     |

use std::collections::HashMap;
use common::FrameTrace;
use ffmpeg_next::{
    codec,
    format::Pixel,
    software::scaling,
    util::frame::video::Video,
    ffi::*,
};
use std::ptr;
use super::{BackendError, FrameOutput, VideoBackend, YuvFrame};

// ─────────────────────────────────────────────────────────────────────────────
// Struct
// ─────────────────────────────────────────────────────────────────────────────

pub struct DesktopFfmpegBackend {
    decoder:  ffmpeg_next::decoder::Video,
    scaler:   Option<scaling::Context>,
    last_fmt: Pixel,
    pending_traces: HashMap<u64, Option<FrameTrace>>,

    // ── Zero-allocation pool ─────────────────────────────────────────────────

    /// Переиспользуемый CPU AVFrame для av_hwframe_transfer_data.
    /// Живёт всё время работы бекенда; av_frame_unref() сбрасывает данные
    /// между кадрами без освобождения и повторного выделения памяти.
    transfer_frame: *mut AVFrame,

    /// Переиспользуемый выходной кадр для SwsScale (только SW-путь).
    scaler_out: Option<Video>,

    /// Пул пиксельных буферов. Swap-паттерн:
    ///   1. resize (no-op если размер совпадает)
    ///   2. copy_from_slice из ffmpeg
    ///   3. mem::replace → отдаём буфер в YuvFrame, в struct кладём пустой Vec
    ///   4. следующий кадр: пустой Vec снова resize до нужного размера
    ///      (первый раз — malloc; далее — realloc не нужен, capacity уже есть)
    y_pool:  Vec<u8>,
    uv_pool: Vec<u8>,
}

// SAFETY: *mut AVFrame управляется исключительно этим потоком.
unsafe impl Send for DesktopFfmpegBackend {}

// ─────────────────────────────────────────────────────────────────────────────
// impl
// ─────────────────────────────────────────────────────────────────────────────

impl DesktopFfmpegBackend {
    /// Инициализировать ffmpeg и открыть HEVC-декодер (VAAPI → software fallback).
    pub fn new() -> Result<Self, BackendError> {
        ffmpeg_next::init()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;
        ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

        let codec = codec::decoder::find(codec::Id::HEVC)
            .ok_or_else(|| BackendError::ConfigError(
                "HEVC codec not found. Install ffmpeg with HEVC support.".into()
            ))?;

        let mut ctx = codec::context::Context::new();

        unsafe {
            let raw = ctx.as_mut_ptr();
            // Убираем внутреннюю буферизацию кадров — минимальная задержка.
            (*raw).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
            // Многопоточность добавляет задержку на одном кадре.
            (*raw).thread_count = 1;
        }

        // ── VAAPI hardware decode ────────────────────────────────────────────
        unsafe {
            let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
            let ret = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            );

            if ret < 0 {
                log::warn!("[Decoder] VAAPI init failed (ret={ret}), using software decode");
            } else {
                log::info!("[Decoder] VAAPI hardware decode active");
                (*ctx.as_mut_ptr()).hw_device_ctx = av_buffer_ref(hw_device_ctx);
                av_buffer_unref(&mut hw_device_ctx);
                (*ctx.as_mut_ptr()).get_format = Some(get_hw_format);
            }
        }

        let decoder = ctx
            .decoder()
            .open_as(codec)
            .map_err(|e| BackendError::ConfigError(e.to_string()))?
            .video()
            .map_err(|e| BackendError::ConfigError(e.to_string()))?;

        // Единственный malloc AVFrame за всё время жизни бекенда.
        let transfer_frame = unsafe { av_frame_alloc() };
        if transfer_frame.is_null() {
            return Err(BackendError::ConfigError("av_frame_alloc failed".into()));
        }

        log::info!("[Decoder] DesktopFfmpegBackend ready");
        Ok(Self {
            decoder,
            scaler:         None,
            last_fmt:       Pixel::None,
            pending_traces: HashMap::new(),
            transfer_frame,
            scaler_out:     None,
            y_pool:         Vec::new(),
            uv_pool:        Vec::new(),
        })
    }
}

impl Drop for DesktopFfmpegBackend {
    fn drop(&mut self) {
        unsafe {
            if !self.transfer_frame.is_null() {
                av_frame_free(&mut self.transfer_frame);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VideoBackend
// ─────────────────────────────────────────────────────────────────────────────

impl VideoBackend for DesktopFfmpegBackend {
    fn push_encoded(
        &mut self,
        payload:  &[u8],
        frame_id: u64,
        trace:    Option<FrameTrace>,
    ) -> Result<(), BackendError> {
        let mut pkt = ffmpeg_next::Packet::new(payload.len());
        if let Some(dst) = pkt.data_mut() {
            dst.copy_from_slice(payload);
        }
        pkt.set_pts(Some(frame_id as i64));
        pkt.set_dts(Some(frame_id as i64));

        self.decoder.send_packet(&pkt)
            .map_err(|e| BackendError::DecodeError(e.to_string()))?;

        self.pending_traces.insert(frame_id, trace);

        // Защита от утечки: чистим трейсы кадров, которые декодер навсегда дропнул.
        const TRACE_HORIZON: u64 = 120;
        if frame_id > TRACE_HORIZON {
            self.pending_traces.retain(|&k, _| k >= frame_id - TRACE_HORIZON);
        }
        Ok(())
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        // ── 1. Получаем декодированный кадр ──────────────────────────────────

        let mut raw = Video::empty();
        if self.decoder.receive_frame(&mut raw).is_err() {
            return Ok(FrameOutput::Pending);
        }

        // ── 2. Если кадр на VAAPI surface — скачиваем в CPU NV12 ─────────────
        //
        // Ключевая оптимизация: вместо av_frame_alloc() + av_frame_free() на
        // каждом кадре используем один предварительно выделенный transfer_frame.
        // av_frame_unref() освобождает ссылки на данные, но не саму структуру.

        let frame_ptr: *const AVFrame = if raw.format() == Pixel::VAAPI {
            unsafe {
                // Сбрасываем данные предыдущего кадра (не освобождает struct).
                av_frame_unref(self.transfer_frame);

                // Запрашиваем NV12 — единственный формат, поддерживаемый нашим шейдером.
                (*self.transfer_frame).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;

                let ret = av_hwframe_transfer_data(
                    self.transfer_frame,
                    raw.as_ptr(),
                    0,
                );
                if ret < 0 {
                    return Err(BackendError::DecodeError(
                        format!("av_hwframe_transfer_data failed: {ret}")
                    ));
                }
                av_frame_copy_props(self.transfer_frame, raw.as_ptr());
                self.transfer_frame as *const AVFrame
            }
        } else {
            unsafe {raw.as_ptr()}
        };

        // ── 3. Читаем метаданные ──────────────────────────────────────────────

        let (frame_id, fmt, w, h) = unsafe {
            let f: &AVFrame = &*frame_ptr;
            let fid = if f.pts != AV_NOPTS_VALUE { f.pts as u64 } else { 0 };
            // ffmpeg_next::format::Pixel::from(i32) через transmute безопасно:
            // AVPixelFormat — repr(C) enum.
            let fmt_sys: AVPixelFormat = std::mem::transmute(f.format);
            let fmt = Pixel::from(fmt_sys);
            (fid, fmt, f.width as u32, f.height as u32)
        };

        let trace = self.pending_traces
            .remove(&frame_id)
            .flatten()
            .unwrap_or_default();
        let mut trace = trace;
        trace.decode_us = FrameTrace::now_us();

        // ── 4. Конвертируем в NV12 если нужно (SW-путь) ──────────────────────

        let nv12_ptr: *const AVFrame = if fmt == Pixel::NV12 {
            // VAAPI путь: кадр уже NV12 в transfer_frame, конвертация не нужна.
            frame_ptr
        } else {
            // SW fallback: конвертируем через SwsScale в переиспользуемый буфер.
            if fmt != self.last_fmt {
                self.last_fmt = fmt;
                self.scaler = Some(
                    scaling::Context::get(
                        fmt, w, h,
                        Pixel::NV12, w, h,
                        scaling::Flags::BILINEAR,
                    )
                    .map_err(|e| BackendError::DecodeError(e.to_string()))?,
                );
                // Переиспользуемый выходной буфер для скалера.
                self.scaler_out = Some(Video::new(Pixel::NV12, w, h));
                log::debug!("[Decoder] Scaler created: {:?} → NV12 {}×{}", fmt, w, h);
            }

            let sc  = self.scaler.as_mut().unwrap();
            let out = self.scaler_out.as_mut().unwrap();

            // Оборачиваем сырой указатель в Video на время вызова scaler.run.
            // SAFETY: frame_ptr валиден на протяжении этого блока.
            let src_video = unsafe { Video::wrap(frame_ptr as *mut AVFrame) };
            sc.run(&src_video, out)
                .map_err(|e| BackendError::DecodeError(e.to_string()))?;
            // Не допускаем drop Video::wrap — он освободит чужой AVFrame.
            std::mem::forget(src_video);

            unsafe {out.as_ptr()}
        };

        // ── 5. Копируем плоскости в пул буферов (ровно одно memcpy на плоскость)

        let (y_stride, uv_stride, y_len, uv_len) = unsafe {
            let f = &*nv12_ptr;
            let ys  = f.linesize[0] as usize;
            let uvs = f.linesize[1] as usize;
            (ys, uvs, ys * h as usize, uvs * h as usize / 2)
        };

        // resize: no-op если capacity уже достаточна (после первого кадра).
        self.y_pool.resize(y_len, 0);
        self.uv_pool.resize(uv_len, 0);

        unsafe {
            let f = &*nv12_ptr;
            self.y_pool[..y_len].copy_from_slice(std::slice::from_raw_parts(f.data[0], y_len));
            self.uv_pool[..uv_len].copy_from_slice(std::slice::from_raw_parts(f.data[1], uv_len));
        }

        // Swap: отдаём заполненные буферы в YuvFrame, в struct кладём пустые Vec.
        // Пустой Vec не аллоцирует (capacity == 0); следующий resize восстановит
        // capacity без malloc, потому что аллокатор вернёт тот же блок.
        let y  = std::mem::replace(&mut self.y_pool,  Vec::new());
        let uv = std::mem::replace(&mut self.uv_pool, Vec::new());

        Ok(FrameOutput::Yuv(YuvFrame {
            frame_id,
            trace,
            width:     w,
            height:    h,
            y,
            uv,
            y_stride:  y_stride  as u32,
            uv_stride: uv_stride as u32,
        }))
    }

    fn shutdown(&mut self) {
        log::info!("[Decoder] DesktopFfmpegBackend: shutdown");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// get_hw_format callback
// ─────────────────────────────────────────────────────────────────────────────

/// Колбэк ffmpeg: выбирает VAAPI из списка форматов, предложенных декодером.
unsafe extern "C" fn get_hw_format(
    _ctx:     *mut AVCodecContext,
    pix_fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    unsafe {
        let mut p = pix_fmts;
        while *p != AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *p;
            }
            p = p.add(1);
        }
        // Не нашли VAAPI — пусть ffmpeg выберет программный формат.
        AVPixelFormat::AV_PIX_FMT_NONE
    }
}