# stream-side

Low-latency game streaming stack:

- `sender`: capture + encode + QUIC transport
- `receiver`: QUIC receive + decode + present
- `common`: shared protocol/FEC/trace types

## Quick Start

### Prerequisites

- Linux sender host with PipeWire + XDG Desktop Portal
- Rust toolchain (`cargo`, `rustc`)
- FFmpeg libraries with hardware codec support (VAAPI/NVENC depending on GPU)

### Build

```sh
cargo build --release
```

### Run sender (Linux)

```sh
# Default listen address
cargo run --release -p sender

# Custom listen address
cargo run --release -p sender -- 0.0.0.0:4433
```

### Run desktop receiver

```sh
cargo run --release -p receiver --features="desktop" -- 127.0.0.1:4433
```

### Build Android receiver app

```sh
cargo ndk -t arm64-v8a -P 26 -o ../android-app/app/src/main/jniLibs build --release
cd android-app
./gradlew assembleRelease
```

APK output:

`android-app/app/build/outputs/apk/release`

## Sender Tuning Features

Recent changes added runtime sender tuning for bitrate, pacing, profile selection, and GOP behavior.

You can configure these by CLI flags and/or environment variables.

### CLI Flags

Use flags after `--`.

```sh
cargo run --release -p sender -- \
	0.0.0.0:4433 \
	--profile balanced \
	--bitrate-mbps 60 \
	--min-bitrate-mbps 25 \
	--max-bitrate-mbps 120 \
	--gop 60 \
	--pacer-mbps 500 \
	--pacer-burst-ms 2.5
```

Available flags:

- `--profile latency|balanced|quality`
- `--bitrate-mbps <int>`
- `--min-bitrate-mbps <int>`
- `--max-bitrate-mbps <int>`
- `--gop <int>`
- `--pacer-mbps <float>`
- `--pacer-burst-ms <float>`
- `--enable-nvidia-dmabuf`

Notes:

- If min bitrate is greater than max bitrate, values are auto-swapped.
- Target bitrate is clamped into `[min, max]`.
- The first positional argument is still the listen address.

### Environment Variables

Equivalent settings:

- `STREAM_SENDER_PROFILE=latency|balanced|quality`
- `STREAM_TARGET_BITRATE_MBPS=<int>`
- `STREAM_MIN_BITRATE_MBPS=<int>`
- `STREAM_MAX_BITRATE_MBPS=<int>`
- `STREAM_GOP_SIZE=<int>`
- `STREAM_PACER_MBPS=<float>`
- `STREAM_PACER_BURST_MS=<float>`
- `STREAM_ENABLE_NVIDIA_DMABUF=true|false`

Example:

```sh
export STREAM_SENDER_PROFILE=balanced
export STREAM_TARGET_BITRATE_MBPS=60
export STREAM_MIN_BITRATE_MBPS=25
export STREAM_MAX_BITRATE_MBPS=120
export STREAM_GOP_SIZE=60
export STREAM_PACER_MBPS=500
export STREAM_PACER_BURST_MS=2.5

cargo run --release -p sender -- 0.0.0.0:4433
```

### Profile Behavior

- `latency`: lower-latency NVENC preset bias (`p2`) and low-latency transport-oriented settings.
- `balanced`: default preset (`p3`) and default stability/quality tradeoff.
- `quality`: higher-quality NVENC preset bias (`p5`) with potentially higher encode latency.

### NVIDIA DMA-BUF Override

`--enable-nvidia-dmabuf` / `STREAM_ENABLE_NVIDIA_DMABUF=true` only controls PipeWire buffer negotiation.

Current state:

- NVENC DMA-BUF ingest path is not fully implemented yet.
- For NVIDIA production runs, keep this disabled unless you are actively testing DMA-BUF path development.

## Performance Benchmarking
1. Build release binaries.

```sh
cargo build --release -p sender -p receiver
```

2. Start sender with an explicit tuning profile for the test case.

```sh
RUST_LOG=info ./target/release/sender \
	0.0.0.0:4433 \
	--profile balanced \
	--bitrate-mbps 60 \
	--min-bitrate-mbps 25 \
	--max-bitrate-mbps 120 \
	--gop 60 \
	--pacer-mbps 500 \
	--pacer-burst-ms 2.5
```

3. Start receiver.

```sh
RUST_LOG=info ./target/release/receiver 127.0.0.1:4433
```

4. Run for 5-10 minutes with representative game scenes:

- static scene
- high motion scene
- high detail scene

5. Capture sender logs and analyze frame-trace totals (`TOTAL=...ms`) and drop/recovery messages.

6. Compare runs across profiles and bitrate/pacer settings.

Suggested metrics to track per run:

- End-to-end latency p50/p95 from frame trace logs
- Receiver drop events (`DROPPED ON PUSH`, `DROPPED ON POLL`)
- Keyframe recovery frequency (`RequestKeyFrame`)
- Subjective quality under fast motion and fine detail

## Practical Tuning Recipes

### 4K60 wired LAN, latency-focused

```sh
./target/release/sender 0.0.0.0:4433 \
	--profile latency \
	--bitrate-mbps 50 \
	--min-bitrate-mbps 30 \
	--max-bitrate-mbps 90 \
	--gop 60 \
	--pacer-mbps 600 \
	--pacer-burst-ms 2.0
```

### 4K60 wired LAN, quality-focused

```sh
./target/release/sender 0.0.0.0:4433 \
	--profile quality \
	--bitrate-mbps 90 \
	--min-bitrate-mbps 50 \
	--max-bitrate-mbps 140 \
	--gop 90 \
	--pacer-mbps 400 \
	--pacer-burst-ms 3.0
```

## Troubleshooting

- If the sender starts but quality is poor: increase target/max bitrate and verify receiver decode path can keep up.
- If latency spikes: lower burst cap, reduce bitrate, and use `latency` profile.
- If frequent keyframe recovery appears: reduce GOP size and verify LAN packet stability.
- If NVIDIA path behaves unexpectedly with DMA-BUF enabled: disable override and use CPU buffer path.
