#!/bin/sh
# Audio-only FFmpeg for NovaPlayer (Windows x86_64, shared, LGPL), cross-built with mingw-w64.
# Same commit as the full BtbN SDK it replaces (N-125146-gc6bb22dea0, avcodec-62 ABI), so
# player-core compiles against identical headers. Runs inside ubuntu:22.04.
# /in/audio-decoders.txt: the audio decoders the full build has (`ffmpeg -decoders`, no lib*);
# tools/ffmpeg-audio/audio-decoders.txt is that list for c6bb22dea0.
# /out receives the SDK layout (include/ lib/ bin/). If /source-out is mounted it receives the
# corresponding source: FFmpeg-$COMMIT.tar.gz, FFmpeg's LGPL text and these scripts.
# make.sh runs it.
set -eu
COMMIT=c6bb22dea0
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends mingw-w64 gcc libc6-dev nasm make pkg-config git ca-certificates >/dev/null
mkdir -p /src && cd /src
if [ ! -d ffmpeg/.git ]; then
    git clone -q --filter=blob:none --no-checkout https://github.com/FFmpeg/FFmpeg.git ffmpeg
fi
cd ffmpeg
git checkout -q "$COMMIT"
git log -1 --format='%h %cd' --date=short

# The full build's audio decoders that this tree names (configure uses '_' where -decoders has '.').
./configure --list-decoders | tr -s ' \t' '\n' | grep -v '^$' | sort -u > /tmp/known.txt
tr '.' '_' < /in/audio-decoders.txt | sort -u | comm -12 - /tmp/known.txt > /tmp/audio.txt
echo "audio decoders: $(wc -l < /tmp/audio.txt)"
DECODERS=$(paste -sd, /tmp/audio.txt)

rm -rf /out/*
./configure \
    --prefix=/out --arch=x86_64 --target-os=mingw32 --cross-prefix=x86_64-w64-mingw32- \
    --enable-shared --disable-static --disable-debug --disable-doc --disable-programs \
    --disable-everything --disable-autodetect --enable-w32threads --enable-schannel \
    --disable-avdevice --disable-swscale \
    --enable-avcodec --enable-avformat --enable-avfilter --enable-swresample \
    --enable-decoder="$DECODERS" \
    --enable-demuxers --enable-parsers --enable-bsfs \
    --enable-network --enable-protocol=file,pipe,data,http,https,tls,tcp,crypto,cache \
    --enable-filter=abuffer,abuffersink,surround,aformat,aresample,anull \
    --extra-ldflags=-static-libgcc \
    >/tmp/configure.log 2>&1 || { tail -40 /tmp/configure.log; exit 1; }
make -j"$(nproc)" >/tmp/make.log 2>&1 || { tail -60 /tmp/make.log; exit 1; }
make install >/dev/null
x86_64-w64-mingw32-strip /out/bin/*.dll
# MSVC import libraries (player-core links with link.exe), as the BtbN SDK ships them.
cd /out/bin
for dll in *.dll; do
    base=${dll%%-*}
    { echo "LIBRARY $dll"; echo EXPORTS; x86_64-w64-mingw32-objdump -p "$dll" \
        | awk '/\[Ordinal\/Name Pointer\] Table/{on=1; next} on && /^\t\[/{print $NF} on && /^$/{on=0}'; } > /tmp/$base.def
    x86_64-w64-mingw32-dlltool -d /tmp/$base.def -l /out/lib/$base.lib -D "$dll"
done
cp /src/ffmpeg/COPYING.LGPLv2.1 /out/LICENSE.txt
if [ -d /source-out ]; then
    git -C /src/ffmpeg archive --format=tar.gz --prefix="FFmpeg-$COMMIT/" -o "/source-out/FFmpeg-$COMMIT.tar.gz" "$COMMIT"
    cp /src/ffmpeg/COPYING.LGPLv2.1 /source-out/FFmpeg-LGPL-2.1.txt
    cp /in/build.sh /in/make.sh /in/audio-decoders.txt /source-out/
fi
echo "configure: $(grep -m1 -o 'enable-decoder' /tmp/configure.log >/dev/null; head -1 /tmp/configure.log)"
ls -la /out/bin /out/lib | grep -E "\.(dll|lib)$"
