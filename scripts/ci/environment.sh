#!/usr/bin/env bash
# Shared release compiler and native media prerequisites for DEV-3 Jenkins.
set -euo pipefail
export PATH="${HOME}/.cargo/bin:${PATH}"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
# DEV-3 registry downloads reject multiplexed HTTP/2 streams.
export CARGO_HTTP_MULTIPLEXING=false
export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/obscura/target"
# The delivered media binaries must not require libvpx on consumer workers.
export VPX_STATIC=1
export PYTHONDONTWRITEBYTECODE=1
[[ $(uname -s):$(uname -m) == Linux:x86_64 ]]
command -v readelf >/dev/null
[[ -r /usr/share/doc/libvpx-dev/copyright ]]
source_revision=$(git rev-parse HEAD)
[[ $source_revision =~ ^[a-f0-9]{40}$ ]]
mkdir -p ci-output
cargo --version
cargo nextest --version
# Use the archive beside the installed pkg-config metadata. Its libdir field
# names /usr/lib, while this distro installs the archive in its multiarch directory.
VPX_LIB_DIR=$(pkg-config --variable=pcfiledir vpx)
[[ $VPX_LIB_DIR == /*/pkgconfig ]]
VPX_LIB_DIR=${VPX_LIB_DIR%/pkgconfig}
VPX_VERSION=$(pkg-config --modversion vpx)
VPX_INCLUDE_DIR=$(pkg-config --variable=includedir vpx)
[[ -f $VPX_LIB_DIR/libvpx.a && -s $VPX_LIB_DIR/libvpx.a && -r $VPX_LIB_DIR/libvpx.a ]]
[[ $VPX_INCLUDE_DIR == /* && -r $VPX_INCLUDE_DIR/vpx/vpx_encoder.h ]]
export VPX_LIB_DIR VPX_VERSION VPX_INCLUDE_DIR
