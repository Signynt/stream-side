//! Sender entry point (Linux).
//!
//! # Usage
//!
//! ```sh
//! # Listen on all interfaces, default port
//! cargo run --bin sender
//!
//! # Custom address
//! cargo run --bin sender -- 0.0.0.0:9999
//! ```
//!
//! Receivers (desktop / Android) connect to `<machine-ip>:<port>`.

use std::{env, net::SocketAddr, sync::Arc};
use sender::{
    capture::{linux::LinuxPipeWireSender, VideoSender},
    quic::QuicServer,
    SenderProfile, SenderTuning,
};

fn parse_runtime_options() -> (SocketAddr, SenderTuning) {
    let mut tuning = SenderTuning::from_env();
    let mut listen_addr_arg: Option<String> = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                if let Some(value) = args.next() {
                    match value.parse::<SenderProfile>() {
                        Ok(profile) => tuning.profile = profile,
                        Err(e) => log::warn!("{e}; keeping existing profile"),
                    }
                }
            }
            "--bitrate-mbps" => {
                if let Some(value) = args.next() {
                    if let Ok(mbps) = value.parse::<i64>() {
                        tuning.target_bitrate_bps = mbps.max(1) * 1_000_000;
                    }
                }
            }
            "--min-bitrate-mbps" => {
                if let Some(value) = args.next() {
                    if let Ok(mbps) = value.parse::<i64>() {
                        tuning.min_bitrate_bps = mbps.max(1) * 1_000_000;
                    }
                }
            }
            "--max-bitrate-mbps" => {
                if let Some(value) = args.next() {
                    if let Ok(mbps) = value.parse::<i64>() {
                        tuning.max_bitrate_bps = mbps.max(1) * 1_000_000;
                    }
                }
            }
            "--gop" => {
                if let Some(value) = args.next() {
                    if let Ok(v) = value.parse::<i32>() {
                        tuning.gop_size = v.max(1);
                    }
                }
            }
            "--pacer-mbps" => {
                if let Some(value) = args.next() {
                    if let Ok(v) = value.parse::<f64>() {
                        tuning.pacer_rate_mbps = v.max(1.0);
                    }
                }
            }
            "--pacer-burst-ms" => {
                if let Some(value) = args.next() {
                    if let Ok(v) = value.parse::<f64>() {
                        tuning.pacer_burst_ms = v.max(0.1);
                    }
                }
            }
            "--enable-nvidia-dmabuf" => {
                tuning.allow_nvidia_dmabuf = true;
            }
            value if listen_addr_arg.is_none() => {
                listen_addr_arg = Some(value.to_string());
            }
            other => {
                log::warn!("Ignoring unknown argument: {other}");
            }
        }
    }

    if tuning.min_bitrate_bps > tuning.max_bitrate_bps {
        std::mem::swap(&mut tuning.min_bitrate_bps, &mut tuning.max_bitrate_bps);
    }
    tuning.target_bitrate_bps = tuning
        .target_bitrate_bps
        .clamp(tuning.min_bitrate_bps, tuning.max_bitrate_bps);

    let listen_addr: SocketAddr = listen_addr_arg
        .as_deref()
        .unwrap_or("0.0.0.0:4433")
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("Invalid address: {e}. Falling back to 0.0.0.0:4433");
            "0.0.0.0:4433".parse().unwrap()
        });

    (listen_addr, tuning)
}

#[tokio::main]
async fn main() {
    env_logger::init();

    // Install the rustls crypto provider exactly once.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // ── Address ──────────────────────────────────────────────────────────────

    let (listen_addr, tuning) = parse_runtime_options();
    let tuning = Arc::new(tuning);

    log::info!("Starting QUIC server on {listen_addr}");
    log::info!("Receivers must connect to <your-ip>:{}", listen_addr.port());
    log::info!(
        "Sender tuning: profile={:?} bitrate={}Mbps ({}-{}), gop={}, pacer={}Mbps burst={}ms nvidia_dmabuf={}",
        tuning.profile,
        tuning.target_bitrate_bps / 1_000_000,
        tuning.min_bitrate_bps / 1_000_000,
        tuning.max_bitrate_bps / 1_000_000,
        tuning.gop_size,
        tuning.pacer_rate_mbps,
        tuning.pacer_burst_ms,
        tuning.allow_nvidia_dmabuf,
    );

    // ── Transport ────────────────────────────────────────────────────────────
    let (idr_tx, idr_rx) = tokio::sync::watch::channel(0u64);
    let server = Arc::new(QuicServer::new(listen_addr, idr_tx, tuning.clone()).await);
    let sink   = server.frame_sink();

    // ── Capture + encode ─────────────────────────────────────────────────────
    //
    // `LinuxPipeWireSender` is the concrete VideoSender implementation for Linux.
    // Swap it for `WindowsSender` or `AndroidSender` on other platforms without
    // touching any other code.

    let sender = LinuxPipeWireSender::new(1, 1, idr_rx, tuning);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received Ctrl-C, shutting down");
        }
        res = sender.run(sink) => {
            match res {
                Ok(())   => log::info!("Sender finished cleanly"),
                Err(e)   => log::error!("Sender error: {e}"),
            }
        }
    }
}
