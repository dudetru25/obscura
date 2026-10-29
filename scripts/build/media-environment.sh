#!/usr/bin/env bash
# Shared static libvpx configuration for Debian/Ubuntu source builds.
set -euo pipefail
export VPX_STATIC=1
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
