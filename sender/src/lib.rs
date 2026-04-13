//! Sender crate — screen capture, HEVC encoding, and QUIC transport.

use std::{env, str::FromStr};
use common::FrameTrace;

/// Pluggable screen-capture + encode pipeline.
pub mod capture;

/// VAAPI HEVC encoder (Linux only).
pub mod encode;

/// QUIC transport server.
pub mod quic;

use std::{sync::atomic::{AtomicBool, AtomicU64, Ordering}, time::Duration};

use tokio::sync::{RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderProfile {
    Latency,
    Balanced,
    Quality,
}

impl FromStr for SenderProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "latency" => Ok(Self::Latency),
            "balanced" => Ok(Self::Balanced),
            "quality" => Ok(Self::Quality),
            other => Err(format!("unknown sender profile: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SenderTuning {
    pub profile: SenderProfile,
    pub target_bitrate_bps: i64,
    pub min_bitrate_bps: i64,
    pub max_bitrate_bps: i64,
    pub gop_size: i32,
    pub pacer_rate_mbps: f64,
    pub pacer_burst_ms: f64,
    pub allow_nvidia_dmabuf: bool,
}

impl Default for SenderTuning {
    fn default() -> Self {
        // Keep existing behavior as default until adaptive control is enabled.
        Self {
            profile: SenderProfile::Balanced,
            target_bitrate_bps: 5_000_000,
            min_bitrate_bps: 3_000_000,
            max_bitrate_bps: 80_000_000,
            gop_size: 120,
            pacer_rate_mbps: 100.0,
            pacer_burst_ms: 4.0,
            allow_nvidia_dmabuf: false,
        }
    }
}

impl SenderTuning {
    pub fn from_env() -> Self {
        fn env_parse<T: FromStr>(key: &str) -> Option<T> {
            let raw = env::var(key).ok()?;
            raw.parse::<T>().ok()
        }

        fn env_bool(key: &str) -> Option<bool> {
            let raw = env::var(key).ok()?;
            match raw.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            }
        }

        let mut tuning = Self::default();

        if let Ok(profile_raw) = env::var("STREAM_SENDER_PROFILE") {
            if let Ok(profile) = SenderProfile::from_str(&profile_raw) {
                tuning.profile = profile;
            }
        }

        if let Some(v) = env_parse::<i64>("STREAM_TARGET_BITRATE_MBPS") {
            tuning.target_bitrate_bps = v.max(1) * 1_000_000;
        }
        if let Some(v) = env_parse::<i64>("STREAM_MIN_BITRATE_MBPS") {
            tuning.min_bitrate_bps = v.max(1) * 1_000_000;
        }
        if let Some(v) = env_parse::<i64>("STREAM_MAX_BITRATE_MBPS") {
            tuning.max_bitrate_bps = v.max(1) * 1_000_000;
        }
        if let Some(v) = env_parse::<i32>("STREAM_GOP_SIZE") {
            tuning.gop_size = v.max(1);
        }
        if let Some(v) = env_parse::<f64>("STREAM_PACER_MBPS") {
            tuning.pacer_rate_mbps = v.max(1.0);
        }
        if let Some(v) = env_parse::<f64>("STREAM_PACER_BURST_MS") {
            tuning.pacer_burst_ms = v.max(0.1);
        }
        if let Some(v) = env_bool("STREAM_ENABLE_NVIDIA_DMABUF") {
            tuning.allow_nvidia_dmabuf = v;
        }

        if tuning.min_bitrate_bps > tuning.max_bitrate_bps {
            std::mem::swap(&mut tuning.min_bitrate_bps, &mut tuning.max_bitrate_bps);
        }
        tuning.target_bitrate_bps = tuning
            .target_bitrate_bps
            .clamp(tuning.min_bitrate_bps, tuning.max_bitrate_bps);

        tuning
    }
}

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
    last_idr_request_us: AtomicU64,
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

    fn should_request_idr(&self, min_interval_ms: u64) -> bool {
        let now_us = FrameTrace::now_us();
        let min_interval_us = min_interval_ms.saturating_mul(1_000);

        let prev = self.last_idr_request_us.load(Ordering::Relaxed);
        if now_us.saturating_sub(prev) < min_interval_us {
            return false;
        }

        self.last_idr_request_us
            .store(now_us, Ordering::Relaxed);
        true
    }
}

/// Token-bucket pacer — spreads the datagram chunks of a frame evenly over
/// time instead of blasting them all in a single tight loop.
///
/// # Why this reduces jitter
///
/// Without pacing, all N chunks of a frame hit the kernel socket buffer in
/// microseconds.  The NIC/switch drains that burst at line rate, but the
/// resulting queue-depth spike adds variable latency (jitter) to *later*
/// packets.  Spreading chunks at a controlled byte-rate prevents that spike.
///
/// # Target rate
///
/// Default: 100 Mbit/s.  At that rate a 1 200-byte chunk is released every
/// ~96 µs; a 20-chunk frame (~24 KB) takes ≈ 2 ms — comfortably within a
/// 16 ms frame budget at 60 fps.  Raising the rate (e.g. to 500 Mbit/s for
/// Gigabit LAN) reduces the pacing delay while still smoothing micro-bursts.
pub struct FramePacer {
    /// Accumulated token credit, in bytes.
    tokens: f64,
    /// Fill rate: bytes per microsecond.
    rate_bytes_per_us: f64,
    /// Wall-clock instant of the last token refill.
    last_refill: tokio::time::Instant,
    /// Maximum burst the pacer will absorb (tokens are capped here).
    burst_cap: f64,
}
 
impl FramePacer {
    /// Create a pacer targeting `rate_mbps` Mbit/s.
    ///
    /// `burst_cap_ms` is the maximum token accumulation during idle periods.
    /// 4 ms is a good default: it allows the very first frame to be sent
    /// without delay while still preventing sustained bursts.
    pub fn new(rate_mbps: f64, burst_cap_ms: f64) -> Self {
        let rate_bytes_per_us = rate_mbps * 1e6 / 8.0 / 1e6; // Mbit/s → bytes/µs
        let burst_cap = rate_bytes_per_us * burst_cap_ms * 1_000.0;
        Self {
            tokens: burst_cap, // start full so the first frame is not delayed
            rate_bytes_per_us,
            last_refill: tokio::time::Instant::now(),
            burst_cap,
        }
    }
 
    /// Consume `bytes` tokens and return how long the caller must wait before
    /// sending.  Returns `Duration::ZERO` when there are enough tokens.
    pub fn consume(&mut self, bytes: usize) -> Duration {
        let now = tokio::time::Instant::now();
        let elapsed_us = (now - self.last_refill).as_micros() as f64;
        self.tokens = (self.tokens + elapsed_us * self.rate_bytes_per_us).min(self.burst_cap);
        self.last_refill = now;
 
        let need = bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            Duration::ZERO
        } else {
            let deficit = need - self.tokens;
            self.tokens = 0.0;
            Duration::from_micros((deficit / self.rate_bytes_per_us) as u64)
        }
    }
}