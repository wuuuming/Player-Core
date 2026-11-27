#!/bin/sh
# Audio-only FFmpeg for NovaPlayer (x86_64, shared, LGPL); Windows uses mingw-w64.
# Same commit as the full BtbN SDK it replaces (N-125146-gc6bb22dea0, avcodec-62 ABI), so
# player-core compiles against identical headers. Runs inside ubuntu:22.04.
# /in/audio-decoders.txt: the audio decoders the full build has (`ffmpeg -decoders`, no lib*);
# tools/ffmpeg-audio/audio-decoders.txt is that list for c6bb22dea0.
# /out receives the SDK layout (include/ lib/ bin/). If /source-out is mounted it receives the
# corresponding source: FFmpeg-$COMMIT.tar.gz, FFmpeg's LGPL text and these scripts.
# make.sh runs it.
set -eu
COMMIT=c6bb22dea018fbf753b70c592a1e346b9a24f074
case ${1:-windows} in
    windows) packages=mingw-w64; target='--target-os=mingw32 --cross-prefix=x86_64-w64-mingw32- --enable-w32threads --enable-schannel' ;;
    linux) packages=libgnutls28-dev; target='--target-os=linux --enable-pthreads --enable-gnutls' ;;
    *) exit 2 ;;
esac
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends "$packages" gcc libc6-dev nasm make pkg-config git ca-certificates >/dev/null
mkdir -p /src && cd /src
if [ -f source.tar.gz ]; then
    test "$(gzip -dc source.tar.gz | git get-tar-commit-id)" = "$COMMIT"
    mkdir -p ffmpeg
    tar xzf source.tar.gz -C ffmpeg --strip-components=1
else
    git init -q ffmpeg
    git -C ffmpeg fetch -q --depth=1 https://github.com/FFmpeg/FFmpeg.git "$COMMIT"
    git -C ffmpeg checkout -q FETCH_HEAD
fi
cd ffmpeg

# The full build's audio decoders that this tree names (configure uses '_' where -decoders has '.').
./configure --list-decoders | tr -s ' \t' '\n' | grep -v '^$' | sort -u > /tmp/known.txt
tr '.' '_' < /in/audio-decoders.txt | sort -u | comm -12 - /tmp/known.txt > /tmp/audio.txt
echo "audio decoders: $(wc -l < /tmp/audio.txt)"
DECODERS=$(paste -sd, /tmp/audio.txt)

rm -rf /out/*
./configure \
    --prefix=/out --arch=x86_64 $target \
    --enable-shared --disable-static --disable-debug --disable-doc --disable-programs \
    --disable-everything --disable-autodetect \
    --disable-avdevice --disable-swscale \
    --enable-avcodec --enable-avformat --enable-avfilter --enable-swresample \
    --enable-decoder="$DECODERS" \
    --enable-demuxers --enable-parsers --enable-bsfs \
    --enable-network --enable-protocol=file,pipe,data,http,https,httpproxy,tls,tcp,crypto,cache \
    --enable-filter=abuffer,abuffersink,surround,aformat,aresample,anull \
    --extra-ldflags=-static-libgcc \
    >/tmp/configure.log 2>&1 || { tail -40 /tmp/configure.log; exit 1; }
make -j"$(nproc)" >/tmp/make.log 2>&1 || { tail -60 /tmp/make.log; exit 1; }
make install >/dev/null
if [ "${1:-windows}" = linux ]; then
    strip --strip-unneeded /out/lib/*.so.*.*.*
else
x86_64-w64-mingw32-strip /out/bin/*.dll
# MSVC import libraries (player-core links with link.exe), as the BtbN SDK ships them.
cd /out/bin
for dll in *.dll; do
    base=${dll%%-*}
    { echo "LIBRARY $dll"; echo EXPORTS; x86_64-w64-mingw32-objdump -p "$dll" \
        | awk '/\[Ordinal\/Name Pointer\] Table/{on=1; next} on && /^\t\[/{print $NF} on && /^$/{on=0}'; } > /tmp/$base.def
    x86_64-w64-mingw32-dlltool -d /tmp/$base.def -l /out/lib/$base.lib -D "$dll"
done
fi
cp /src/ffmpeg/COPYING.LGPLv2.1 /out/LICENSE.txt
if [ -d /source-out ]; then
    if [ -f /src/source.tar.gz ]; then
        cp /src/source.tar.gz /source-out/FFmpeg-c6bb22dea0.tar.gz
    else
        git -C /src/ffmpeg archive --format=tar.gz --prefix="FFmpeg-c6bb22dea0/" -o /source-out/FFmpeg-c6bb22dea0.tar.gz "$COMMIT"
    fi
    cp /src/ffmpeg/COPYING.LGPLv2.1 /source-out/FFmpeg-LGPL-2.1.txt
    cp /in/build.sh /in/make.sh /in/audio-decoders.txt /source-out/
fi
echo "configure: $(grep -m1 -o 'enable-decoder' /tmp/configure.log >/dev/null; head -1 /tmp/configure.log)"
ls -l /out/lib
