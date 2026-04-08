#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo bench -p sender --bench encode_profile -- "$@"
