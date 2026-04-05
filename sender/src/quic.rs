// src/quic.rs
//
// QUIC-сервер на стороне отправителя.
//
// Архитектура:
//
//   Encoder thread
//       │  try_send(Vec<u8>)          ← sync, O(1), без блокировки
//       ▼
//   mpsc::channel  (буфер 4 кадра)
//       │
//       ▼
//   serializer task                   ← сериализует VideoPacket ОДИН РАЗ
//       │  watch::send(Arc<Vec<u8>>)  ← автоматически дропает старый кадр
//       ▼
//   watch::channel
//       ├──▶ client task A  ──▶ persistent SendStream A
//       ├──▶ client task B  ──▶ persistent SendStream B
//       └──▶ ...
//
// Ключевые решения:
//   - ONE stream per connection:  вместо open_uni() на каждый кадр —
//     открываем стрим один раз при подключении и пишем в него бесконечно.
//   - Arc<Vec<u8>>: сериализованный пакет живёт в Arc, клиентские таски
//     получают только Arc::clone() — O(1), без копирования данных.
//   - watch вместо broadcast: watch автоматически выбрасывает старые кадры,
//     если клиент не успевает — именно то что нужно для low-latency видео.

use quinn::{Endpoint, ServerConfig};
use quinn::crypto::rustls::QuicServerConfig;
use std::sync::Arc;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::watch;
use common::VideoPacket;
use std::time::{SystemTime, UNIX_EPOCH};
// ─────────────────────────────────────────────────────────────────────────────

pub struct QuicServer {
    /// Отправитель закодированных HEVC-кадров.
    /// Используется из синхронного потока энкодера через try_send.
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl QuicServer {
    /// Запустить QUIC-сервер, привязанный к `listen_addr`.
    ///
    /// Возвращает немедленно — все async-задачи работают в фоне.
    pub async fn new(listen_addr: SocketAddr) -> Self {
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);

        // watch несёт уже сериализованный + length-prefixed пакет
        let (watch_tx, watch_rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);

        // ── Задача сериализации ──────────────────────────────────────────────
        //
        // Сериализуем VideoPacket ОДИН РАЗ для всех клиентов.
        // Результат (length-prefix + postcard-байты) упаковывается в Arc,
        // чтобы клиентские таски могли дёшево его клонировать.
        tokio::spawn(async move {
            let mut frame_id = 0u64;

            while let Some(payload) = frame_rx.recv().await {
                frame_id += 1;
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let packet = VideoPacket {
                    frame_id,
                    payload,
                    timestamp,
                };

                let encoded = match postcard::to_allocvec(&packet) {
                    Ok(b)  => b,
                    Err(e) => { eprintln!("❌ Serialization error: {e}"); continue; }
                };

                // length-prefix (4 байта LE) + тело
                let mut msg = Vec::with_capacity(4 + encoded.len());
                msg.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
                msg.extend_from_slice(&encoded);

                // watch автоматически вытеснит старый кадр если клиент тупит
                let _ = watch_tx.send(Some(Arc::new(msg)));
            }
        });

        // ── Сервер ──────────────────────────────────────────────────────────
        let endpoint = build_server_endpoint(listen_addr);
        eprintln!("🎯 QUIC-сервер слушает на {}", listen_addr);

        tokio::spawn(async move {
            while let Some(connecting) = endpoint.accept().await {
                let rx = watch_rx.clone();

                tokio::spawn(async move {
                    match connecting.await {
                        Ok(conn) => {
                            let remote = conn.remote_address();
                            eprintln!("✅ Клиент подключился: {}", remote);

                            // Открываем ОДИН персистентный стрим для этого клиента
                            match conn.open_uni().await {
                                Ok(stream) => send_to_client(stream, rx).await,
                                Err(e)     => eprintln!("❌ open_uni для {remote}: {e}"),
                            }

                            eprintln!("📤 Клиент отключился: {}", remote);
                        }
                        Err(e) => eprintln!("❌ Handshake failed: {e}"),
                    }
                });
            }
        });

        Self { tx: frame_tx }
    }

    /// Поставить закодированный HEVC-кадр в очередь отправки.
    ///
    /// Вызывается из синхронного потока энкодера.
    /// Если очередь заполнена — кадр **молча выбрасывается** (backpressure).
    pub fn send(&self, data: Vec<u8>) {
        use tokio::sync::mpsc::error::TrySendError;
        match self.tx.try_send(data) {
            Ok(_) => {}
            Err(TrySendError::Full(_))   => { /* сеть не успевает — дропаем */ }
            Err(TrySendError::Closed(_)) => eprintln!("❌ QuicServer channel closed"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Обслуживание одного клиента
// ─────────────────────────────────────────────────────────────────────────────

/// Читает кадры из watch и пишет их в персистентный SendStream.
///
/// Функция возвращается когда клиент отключается или watch-канал закрыт.
async fn send_to_client(
    mut stream: quinn::SendStream,
    mut rx:     watch::Receiver<Option<Arc<Vec<u8>>>>,
) {
    loop {
        // Ждём изменения (нового кадра)
        if rx.changed().await.is_err() {
            break; // watch::Sender дропнут — сервер завершил работу
        }

        // Arc::clone() — O(1), данные не копируются
        let msg = rx.borrow_and_update().clone();

        if let Some(msg) = msg {
            if stream.write_all(&msg).await.is_err() {
                break; // клиент отключился
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Создание QUIC server endpoint с self-signed сертификатом
// ─────────────────────────────────────────────────────────────────────────────

fn build_server_endpoint(addr: SocketAddr) -> Endpoint {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("Failed to generate TLS certificate");

    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        cert.signing_key.serialize_der().into(),
    );
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key)
        .expect("TLS ServerConfig error");
    tls.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicServerConfig::try_from(tls)
        .expect("QuicServerConfig error");
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    // Разрешаем миграцию путей (полезно для Wi-Fi/Tailscale)
    transport.keep_alive_interval(Some(Duration::from_secs(3)));
    
    server_cfg.transport_config(Arc::new(transport));

    Endpoint::server(server_cfg, addr).expect("Failed to bind QUIC server")
}
