// src/network.rs
//
// Кроссплатформенный сетевой цикл приёма видеопотока.
//
// Этот файл ОДИНАКОВ для всех платформ — он ничего не знает о том,
// как именно происходит декодирование и рендеринг. Всё, что он делает:
//
//   1. Принимает QUIC-соединение
//   2. Читает length-prefixed пакеты
//   3. Десериализует VideoPacket через postcard
//   4. Вызывает backend.push_encoded(payload, frame_id)
//   5. Вызывает backend.poll_output() в цикле для дренажа
//
// На десктопе результат poll_output() — YuvFrame — уходит в mpsc-канал
// к рендер-потоку (winit loop).
// На Android poll_output() вызывается исключительно для того, чтобы
// releaseOutputBuffer отправил кадр в Surface; возвращаемое DirectToSurface
// просто игнорируется.

use std::error::Error;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use common::VideoPacket;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{ClientConfig, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, ServerName};

use crate::backend::{FrameOutput, VideoBackend};

// ─────────────────────────────────────────────────────────────────────────────
// Точка входа (для обеих платформ)
// ─────────────────────────────────────────────────────────────────────────────

/// Запустить QUIC-ресивер.
///
/// - `backend`  — платформо-специфичный декодер, обёрнутый в `Arc<Mutex<>>`
///               для безопасной передачи между потоками.
/// - `frame_tx` — `Some(sender)` на десктопе (YUV-кадры идут в рендер),
///               `None` на Android (кадры уходят в Surface, канал не нужен).
/// - `addr`     — адрес сервера (sender'а на ПК): "192.168.1.5:4433"
pub async fn run_quic_receiver<B: VideoBackend>(
    backend:  Arc<Mutex<B>>,
    addr:     SocketAddr,
    frame_tx: Option<mpsc::SyncSender<crate::backend::YuvFrame>>,
) -> Result<(), Box<dyn Error>> { // <--- ТЕПЕРЬ ВОЗВРАЩАЕМ RESULT
    
    rustls::crypto::ring::default_provider().install_default().ok();
    // 1. Извлекаем endpoint. Теперь ? работает, так как функция возвращает Result
    let endpoint = build_quic_endpoint(addr)?;

    eprintln!("🚀 Ресивер запущен на {}", addr);

    // 2. Принимаем соединения
    while let Some(connecting) = endpoint.accept().await {
        let backend_clone = backend.clone();
        let frame_tx_clone = frame_tx.clone();

        tokio::spawn(async move {
            // 3. РАСПАКОВЫВАЕМ соединение (избавляемся от Result)
            match connecting.await {
                Ok(connection) => {
                    log::info!("[QUIC] Connected: {}", connection.remote_address());
                    
                    // 4. Теперь вызываем accept_uni у чистой connection
                    while let Ok(mut stream) = connection.accept_uni().await {
                        log::debug!("[QUIC] New stream");
                        handle_stream(&mut stream, backend_clone.clone(), frame_tx_clone.clone()).await;
                    }
                }
                Err(e) => log::error!("[QUIC] Handshake failed: {}", e),
            }
        });
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Обработка одного QUIC-стрима
// ─────────────────────────────────────────────────────────────────────────────

async fn handle_stream<B: VideoBackend>(
    stream:   &mut quinn::RecvStream,
    backend:  Arc<Mutex<B>>,
    frame_tx: Option<mpsc::SyncSender<crate::backend::YuvFrame>>,
) {
    let mut len_buf = [0u8; 4];

    loop {
        // ── Читаем 4-байтный length-prefix ───────────────────────────────
        if stream.read_exact(&mut len_buf).await.is_err() {
            break; // стрим закрыт
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        if len == 0 || len > 10_000_000 {
            log::warn!("[QUIC] Suspicious packet length: {len}");
            break;
        }

        // ── Читаем тело пакета ────────────────────────────────────────────
        let mut buf = vec![0u8; len];
        if stream.read_exact(&mut buf).await.is_err() {
            break;
        }

        // ── Десериализация через postcard ─────────────────────────────────
        let packet: VideoPacket = match postcard::from_bytes(&buf) {
            Ok(p)  => p,
            Err(e) => {
                log::error!("[QUIC] Deserialization error: {e}");
                continue;
            }
        };

        // Логируем задержку каждые 60 кадров
        if let Ok(elapsed) = packet.send_time.elapsed() {
            if packet.frame_id % 60 == 0 {
                log::info!(
                    "Frame #{} | latency={}ms | size={}KB",
                    packet.frame_id,
                    elapsed.as_millis(),
                    packet.payload.len() / 1024,
                );
            }
        }

        // ── Передаём payload в декодер ────────────────────────────────────
        //
        // Блокируем мьютекс на минимальное время.
        // На Android это занимает ~1-5 µs (копия данных в MediaCodec buffer).
        // На десктопе — ~2-10 µs (копия в ffmpeg packet).
        {
            let mut b = backend.lock().unwrap();

            match b.push_encoded(&packet.payload, packet.frame_id) {
                Ok(()) => {}
                Err(crate::backend::BackendError::BufferFull) => {
                    // Декодер перегружен — пропускаем кадр.
                    // Это нормально при временных пиках нагрузки.
                    log::warn!("[Decoder] Buffer full, dropping frame #{}", packet.frame_id);
                    continue;
                }
                Err(e) => {
                    log::error!("[Decoder] push_encoded error: {e}");
                    continue;
                }
            }

            // ── Дренируем выходную очередь декодера ──────────────────────
            //
            // Декодер может иметь несколько кадров в очереди — дренируем все.
            //
            // Android: каждый вызов poll_output() с кадром вызывает
            //          releaseOutputBuffer(render=true), что рендерит кадр в Surface.
            //          Без этого цикла MediaCodec заблокируется после заполнения
            //          своей выходной очереди.
            //
            // Desktop: YUV-кадры отправляются в канал рендер-потока.
            loop {
                match b.poll_output() {
                    Ok(FrameOutput::Yuv(frame)) => {
                        if let Some(ref tx) = frame_tx {
                            // try_send не блокирует: если очередь полна —
                            // пропускаем, не задерживая сетевой поток
                            let _ = tx.try_send(frame);
                        }
                        // Продолжаем дренировать
                    }
                    Ok(FrameOutput::DirectToSurface) => {
                        // Android: кадр отрендерен — продолжаем дренировать
                    }
                    Ok(FrameOutput::Pending) | Err(_) => break, // очередь пуста
                }
            }
        } // мьютекс отпущен
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QUIC endpoint с skip-verify TLS (для LAN / self-signed)
// ─────────────────────────────────────────────────────────────────────────────

fn build_quic_endpoint(addr: SocketAddr) -> Result<Endpoint, Box<dyn std::error::Error>> {
    // В LAN-сценарии стриминга сервер использует self-signed сертификат.
    // Для продакшна здесь должна быть реальная верификация.
    let (cert, key) = generate_self_signed_cert().unwrap();
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key).unwrap();

    server_config.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicServerConfig::try_from(server_config).unwrap();
    let client_config = ServerConfig::with_crypto(Arc::new(quic_crypto));

    let endpoint = Endpoint::server(client_config, addr).unwrap();
    Ok(endpoint)
}

/// TLS-верификатор, принимающий любые сертификаты.
/// Используется только в разработке/LAN!
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct)
        -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct)
        -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
fn generate_self_signed_cert() -> Result<(rustls::pki_types::CertificateDer<'static>, rustls::pki_types::PrivateKeyDer<'static>), Box<dyn Error>> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    Ok((cert_der, key))
}