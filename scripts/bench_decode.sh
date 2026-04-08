#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo bench -p receiver --bench decode_profile -- "$@"
