// src/backend/android.rs
//
// Android-бекенд: аппаратный HEVC-декодер через NDK AMediaCodec.
#[link(name = "mediandk")]
unsafe extern "C" {}

#[link(name = "android")]
unsafe extern "C" {}

use std::ffi::CString;
use std::ptr;
use std::sync::{Arc, Mutex};

use jni::{
    objects::{JClass, JObject, JString},
    sys::{jint, jobject},
    JNIEnv,
    EnvOutcome
};
use once_cell::sync::OnceCell;

use super::{BackendError, FrameOutput, VideoBackend};

// ─────────────────────────────────────────────────────────────────────────────
// Глобальный синглтон бекенда
// ─────────────────────────────────────────────────────────────────────────────
static BACKEND: OnceCell<Arc<Mutex<AndroidMediaCodecBackend>>> = OnceCell::new();

fn get_or_create_backend() -> Arc<Mutex<AndroidMediaCodecBackend>> {
    BACKEND
        .get_or_init(|| Arc::new(Mutex::new(AndroidMediaCodecBackend::new())))
        .clone()
}

// ─────────────────────────────────────────────────────────────────────────────
// Внутреннее состояние NDK-объектов
// ─────────────────────────────────────────────────────────────────────────────
struct CodecState {
    codec:         *mut ndk_sys::AMediaCodec,
    native_window: *mut ndk_sys::ANativeWindow,
}

unsafe impl Send for CodecState {}
unsafe impl Sync for CodecState {}

impl Drop for CodecState {
    fn drop(&mut self) {
        unsafe {
            ndk_sys::AMediaCodec_stop(self.codec);
            ndk_sys::AMediaCodec_delete(self.codec);
            ndk_sys::ANativeWindow_release(self.native_window);
        }
        log::info!("[MediaCodec] Resources released");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Основная структура бекенда
// ─────────────────────────────────────────────────────────────────────────────
pub struct AndroidMediaCodecBackend {
    state:        Option<CodecState>,
    frame_pts_us: u64,
}

impl AndroidMediaCodecBackend {
    pub fn new() -> Self {
        Self {
            state:        None,
            frame_pts_us: 0,
        }
    }

    pub unsafe fn init_with_surface(
        &mut self,
        native_window: *mut ndk_sys::ANativeWindow,
        width:         i32,
        height:        i32,
    ) -> Result<(), BackendError> {
        self.shutdown();

        let mime   = CString::new("video/hevc").unwrap();
        let codec  = ndk_sys::AMediaCodec_createDecoderByType(mime.as_ptr());
        if codec.is_null() {
            return Err(BackendError::ConfigError(
                "AMediaCodec_createDecoderByType(video/hevc) returned null.".into()
            ));
        }

        let format = ndk_sys::AMediaFormat_new();

        macro_rules! fmt_str {
            ($k:expr, $v:expr) => {{
                let k = CString::new($k).unwrap();
                let v = CString::new($v).unwrap();
                ndk_sys::AMediaFormat_setString(format, k.as_ptr(), v.as_ptr());
            }};
        }
        macro_rules! fmt_i32 {
            ($k:expr, $v:expr) => {{
                let k = CString::new($k).unwrap();
                ndk_sys::AMediaFormat_setInt32(format, k.as_ptr(), $v);
            }};
        }

        fmt_str!("mime",              "video/hevc");
        fmt_i32!("width",             width);
        fmt_i32!("height",            height);
        fmt_i32!("max-input-size",    4 * 1024 * 1024);
        fmt_i32!("adaptive-playback", 1);
        fmt_i32!("low-latency",       1);
        fmt_i32!("priority",          0);
        fmt_i32!("operating-rate",    60);

        let status = ndk_sys::AMediaCodec_configure(
            codec, format, native_window, ptr::null_mut(), 0,
        );
        ndk_sys::AMediaFormat_delete(format);

        if status != ndk_sys::media_status_t(0) {
            ndk_sys::AMediaCodec_delete(codec);
            return Err(BackendError::ConfigError(
                format!("AMediaCodec_configure failed with status={status:?}")
            ));
        }

        let status = ndk_sys::AMediaCodec_start(codec);
        if status != ndk_sys::media_status_t(0) {
            ndk_sys::AMediaCodec_delete(codec);
            return Err(BackendError::ConfigError(
                format!("AMediaCodec_start failed with status={status:?}")
            ));
        }

        self.state = Some(CodecState { codec, native_window });
        log::info!("[MediaCodec] Initialized: {width}x{height} HEVC decoder");
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Реализация трейта VideoBackend
// ─────────────────────────────────────────────────────────────────────────────

impl VideoBackend for AndroidMediaCodecBackend {
    fn push_encoded(&mut self, payload: &[u8], frame_id: u64) -> Result<(), BackendError> {
        let state = self.state.as_ref().ok_or(BackendError::NotInitialized)?;

        unsafe {
            let idx = ndk_sys::AMediaCodec_dequeueInputBuffer(state.codec, 5_000);

            if idx == ndk_sys::AMEDIACODEC_INFO_TRY_AGAIN_LATER as isize {
                return Err(BackendError::BufferFull);
            }
            if idx < 0 {
                return Err(BackendError::DecodeError(format!("dequeue error: {idx}")));
            }
            let idx = idx as usize;

            let mut buf_size: usize = 0;
            let buf_ptr = ndk_sys::AMediaCodec_getInputBuffer(state.codec, idx, &mut buf_size);
            if buf_ptr.is_null() {
                return Err(BackendError::DecodeError("getInputBuffer returned null".into()));
            }

            let copy_len = payload.len().min(buf_size);
            ptr::copy_nonoverlapping(payload.as_ptr(), buf_ptr, copy_len);

            self.frame_pts_us = frame_id.saturating_mul(16_667);

            let status = ndk_sys::AMediaCodec_queueInputBuffer(
                state.codec, idx, 0, copy_len, self.frame_pts_us, 0,
            );
            if status != ndk_sys::media_status_t(0) {
                return Err(BackendError::DecodeError(
                    format!("queueInputBuffer failed: {status:?}")
                ));
            }
        }
        Ok(())
    }

    fn poll_output(&mut self) -> Result<FrameOutput, BackendError> {
        let state = self.state.as_ref().ok_or(BackendError::NotInitialized)?;

        unsafe {
            let mut info = ndk_sys::AMediaCodecBufferInfo {
                offset: 0, size: 0, presentationTimeUs: 0, flags: 0,
            };

            let idx = ndk_sys::AMediaCodec_dequeueOutputBuffer(state.codec, &mut info, 0);

            match idx {
                i if i == ndk_sys::AMEDIACODEC_INFO_TRY_AGAIN_LATER as isize => Ok(FrameOutput::Pending),
                i if i == ndk_sys::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED as isize => Ok(FrameOutput::Pending),
                idx if idx >= 0 => {
                    ndk_sys::AMediaCodec_releaseOutputBuffer(state.codec, idx as usize, true);
                    Ok(FrameOutput::DirectToSurface)
                }
                _ => Ok(FrameOutput::Pending),
            }
        }
    }

    fn shutdown(&mut self) {
        self.state = None;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JNI EXPORTS
// ─────────────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_streamreceiver_NativeLib_initBackend(
    env:     *mut jni::sys::JNIEnv, // ПРОСТО СЫРОЙ УКАЗАТЕЛЬ! Никаких мучений с jni-rs
    _class:  JClass,
    surface: JObject,
    width:   jint,
    height:  jint,
) {
    let _ = std::panic::catch_unwind(|| {
        let native_window = unsafe {
            // Передаем сырой C-указатель напрямую в NDK
            ndk_sys::ANativeWindow_fromSurface(
                env as *mut _, 
                surface.as_raw() as *mut _,
            )
        };

        if native_window.is_null() {
            log::error!("[JNI] ANativeWindow_fromSurface returned null!");
            return;
        }

        let backend = get_or_create_backend();
        let mut guard = backend.lock().unwrap();

        match unsafe { guard.init_with_surface(native_window, width, height) } {
            Ok(())   => log::info!("[JNI] initBackend OK"),
            Err(e)   => log::error!("[JNI] initBackend failed: {e}"),
        }
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_example_streamreceiver_NativeLib_startNetworking(
    mut _env: jni::JNIEnv, // Используем правильный тип JNIEnv
    _class: JClass,
    _unused_host: JString, // Игнорируем, так как мы сервер
    port: jint,
) {
    let _ = std::panic::catch_unwind(|| {
        let backend = get_or_create_backend();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
            rt.block_on(async move {
                // Сервер всегда слушает на 0.0.0.0
                let addr_str = format!("0.0.0.0:{}", port);
                match addr_str.parse() {
                    Ok(addr) => {
                        log::info!("[Network] Starting QUIC server on {}", addr_str);
                        // Запускаем ресивер (сервер) без канала (frame_tx = None)
                        if let Err(e) = crate::network::run_quic_receiver(backend, addr, None).await {
                            log::error!("[Network] Server error: {}", e);
                        }
                    }
                    Err(e) => log::error!("[Network] Invalid bind address {}: {}", addr_str, e),
                }
            });
        });
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_streamreceiver_NativeLib_shutdownBackend(
    _env:   *mut jni::sys::JNIEnv, // Тоже сырой указатель для простоты
    _class: JClass,
) {
    let _ = std::panic::catch_unwind(|| {
        if let Some(backend) = BACKEND.get() {
            backend.lock().unwrap().shutdown();
            log::info!("[JNI] shutdownBackend OK");
        }
    });
}