#!/bin/sh
# The audio-only FFmpeg SDK for Windows, built from FFmpeg's own source: build.sh in Docker's
# ubuntu:22.04 (any Linux host with Docker, or WSL). Its result is what
# crates/player-core/third_party/ffmpeg holds (include/, lib/*.lib, bin/*.dll, LICENSE.txt);
# rerun this to rebuild or update it.
#
#   sh tools/ffmpeg-audio/make.sh <sdk-dir> [<source-dir>]
#
# <sdk-dir> receives include/ lib/ bin/ LICENSE.txt (use it as
# `<NOVA_MEDIA_NATIVE_DIR>/ffmpeg-master-latest-win64-lgpl-shared`). <source-dir>, if given,
# receives what the LGPL asks a distributor of these DLLs to offer: FFmpeg's source at the
# commit (FFmpeg-<commit>.tar.gz), its licence, and this directory's scripts.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
sdk=$(cd "$1" && pwd)
work=$(mktemp -d)
srcmount=""
if [ $# -ge 2 ]; then
    mkdir -p "$2"
    srcmount="-v $(cd "$2" && pwd):/source-out"
fi
# shellcheck disable=SC2086
docker run --rm -v "$here:/in:ro" -v "$sdk:/out" -v "$work:/src" $srcmount ubuntu:22.04 sh /in/build.sh
