#!/usr/bin/env bash
# Shared release compiler and native media prerequisites for DEV-3 Jenkins.
set -euo pipefail
export PATH="${HOME}/.cargo/bin:${PATH}"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
# DEV-3 registry downloads reject multiplexed HTTP/2 streams.
export CARGO_HTTP_MULTIPLEXING=false
export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/obscura/target"
export PYTHONDONTWRITEBYTECODE=1
[[ $(uname -s):$(uname -m) == Linux:x86_64 ]]
command -v readelf >/dev/null
[[ -r /usr/share/doc/libvpx-dev/copyright ]]
source_revision=$(git rev-parse HEAD)
[[ $source_revision =~ ^[a-f0-9]{40}$ ]]
mkdir -p ci-output
cargo --version
cargo nextest --version
source scripts/ci/media-environment.sh
