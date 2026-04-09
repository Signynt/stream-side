//! Sender crate — screen capture, HEVC encoding, and QUIC transport.

/// Pluggable screen-capture + encode pipeline.
pub mod capture;

/// VAAPI HEVC encoder (Linux only).
#[cfg(target_os = "linux")]
pub mod encode;

/// QUIC transport server.
pub mod quic;

use std::sync::atomic::AtomicBool;

use tokio::sync::{RwLock};

#[derive(Debug, Clone, Default)]
struct ClientIdentity {
    model: Option<String>,
    os: Option<String>,
    ready: bool,
}

#[derive(Default)]
struct ConnectionInfo {
    remote: String,
    label: RwLock<String>,
    ready: AtomicBool,
}

impl ConnectionInfo {
    async fn label(&self) -> String {
        let label = self.label.read().await.clone();
        if label.is_empty() {
            self.remote.clone()
        } else {
            label
        }
    }
}