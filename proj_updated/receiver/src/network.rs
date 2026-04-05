// src/network.rs
//
// Кроссплатформенный сетевой цикл приёма видеопотока.
//
// Receiver теперь — QUIC-КЛИЕНТ: подключается к sender'у (серверу).
//
// Поток исполнения:
//
//   run_quic_receiver(sender_addr)
//       │
//       ├── reconnect loop
//       │       │
//       │       ├── endpoint.connect(sender_addr) → connection
//       │       │
//       │       └── connection.accept_uni() → RecvStream
//       │               │
//       │               └── handle_stream (читает пакеты в цикле до закрытия стрима)
//       │                       │
//       │                       ├── backend.push_encoded(payload)
//       │                       └── backend.poll_output() → frame_tx или Surface
//       │
//       └── при потере соединения — пауза 2с → reconnect

use std::error::Error;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use common::VideoPacket;
use quinn::{ClientConfig, Endpoint};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::DigitallySignedStruct;

use crate::backend::{FrameOutput, VideoBackend};

// ─────────────────────────────────────────────────────────────────────────────
// Точка входа
// ─────────────────────────────────────────────────────────────────────────────

/// Запустить QUIC-клиент и подключиться к sender'у на `sender_addr`.
///
/// При потере соединения автоматически переподключается.
///
/// - `backend`   — платформо-специфичный декодер.
/// - `sender_addr` — адрес QUIC-сервера (sender): "192.168.1.5:4433"
/// - `frame_tx`  — `Some(tx)` на десктопе, `None` на Android.
pub async fn run_quic_receiver<B: VideoBackend>(
    backend:     Arc<Mutex<B>>,
    sender_addr: SocketAddr,
    frame_tx:    Option<mpsc::SyncSender<crate::backend::YuvFrame>>,
) -> Result<(), Box<dyn Error>> {
    rustls::crypto::ring::default_provider().install_default().ok();

    loop {
        eprintln!("🔌 Подключаемся к sender'у {}...", sender_addr);

        let endpoint = match build_quic_client_endpoint() {
            Ok(ep) => ep,
            Err(e) => {
                eprintln!("❌ Не удалось создать QUIC endpoint: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let connection = match endpoint.connect(sender_addr, "localhost") {
            Ok(connecting) => match connecting.await {
                Ok(conn) => conn,
                Err(e)   => {
                    eprintln!("❌ Ошибка подключения к {sender_addr}: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
            Err(e) => {
                eprintln!("❌ connect() error: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        eprintln!("✅ Подключились к {}!", connection.remote_address());

        // Sender открывает uni-стримы и посылает в них данные.
        // Нам нужно их принимать.
        while let Ok(mut stream) = connection.accept_uni().await {
            let backend_clone  = backend.clone();
            let frame_tx_clone = frame_tx.clone();

            tokio::spawn(async move {
                handle_stream(&mut stream, backend_clone, frame_tx_clone).await;
            });
        }

        eprintln!("⚠️  Соединение разорвано, переподключаемся через 2с...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
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
        {
            let mut b = backend.lock().unwrap();

            match b.push_encoded(&packet.payload, packet.frame_id) {
                Ok(()) => {}
                Err(crate::backend::BackendError::BufferFull) => {
                    log::warn!("[Decoder] Buffer full, dropping frame #{}", packet.frame_id);
                    continue;
                }
                Err(e) => {
                    log::error!("[Decoder] push_encoded error: {e}");
                    continue;
                }
            }

            // ── Дренируем выходную очередь декодера ──────────────────────
            loop {
                match b.poll_output() {
                    Ok(FrameOutput::Yuv(frame)) => {
                        if let Some(ref tx) = frame_tx {
                            let _ = tx.try_send(frame);
                        }
                    }
                    Ok(FrameOutput::DirectToSurface) => {}
                    Ok(FrameOutput::Pending) | Err(_) => break,
                }
            }
        } // мьютекс отпущен
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QUIC client endpoint (skip-verify для LAN / self-signed)
// ─────────────────────────────────────────────────────────────────────────────

fn build_quic_client_endpoint() -> Result<Endpoint, Box<dyn Error>> {
    let mut crypto = rustls::ClientConfig::builder_with_provider(
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
    .with_no_client_auth();

    crypto.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicClientConfig::try_from(crypto)?;
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(quic_crypto)));

    Ok(endpoint)
}

use std::sync::Arc;

/// TLS-верификатор, принимающий любые сертификаты.
/// Используется только в разработке / LAN!
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity:    &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name:   &ServerName<'_>,
        _ocsp_response: &[u8],
        _now:           rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
