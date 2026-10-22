<h1 align="center">Player-Core</h1>

<p align="center"><b>The audio playback core behind NovaPlayer, in Rust.</b></p>

<p align="center">
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue" alt="License"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux-lightgrey" alt="Platforms">
</p>

<p align="center">English | <a href="README.zh-CN.md">简体中文</a></p>

Player-Core is the part of [NovaPlayer](https://github.com/wuuuming/NovaPlayer) that makes the
sound. It opens a song, decodes it and plays it to a device: on Windows through FFmpeg and WASAPI
or ASIO, on Linux through the system's FFmpeg and PulseAudio (or PipeWire's pulse server). It has
no UI of its own, and NovaPlayer is its only host so far, but nothing in it knows about NovaPlayer.

What it does:

- Decodes whatever FFmpeg decodes, plus TAK, and DSD (sent over ASIO natively or as DoP). Reads CUE
  sheets, and tags and lyrics in legacy code pages.
- Plays gaplessly: the next song is opened ahead and continues the same stream, sample-exact.
  Crossfades when asked to, with equal-power gains.
- Follows the hardware: playback moves with Windows' default output and goes back to a chosen
  device when it is plugged in again.
- Runs one processing chain on both platforms: ReplayGain with a peak limiter, CLAP effect
  plug-ins hosted in process, and a volume smoothed per frame so it never zips.
  [docs/processing.md](docs/processing.md) has the details.
- Reads local files, HTTP(S), or any byte source the host implements (`media-io`, which builds
  without FFmpeg).

[docs/map.md](docs/map.md) says where everything lives.

## Crates

| Crate | |
|---|---|
| `player-core` | the core: probing, decoding, output, processing |
| `media-io` | byte sources and the audio source factories a host implements; no FFmpeg |

## Building

```sh
cargo build --release                              # Windows or Linux, nothing to set up first
cargo build --release --features asio              # Windows, with ASIO (needs LLVM's libclang)
python tools/ci.py                                 # what CI runs: fmt, build, clippy, unit tests
```

On Windows the build links the audio-only FFmpeg in `crates/player-core/third_party/ffmpeg/`
(FFmpeg c6bb22dea0, LGPL, five DLLs, ~9 MB), which `tools/ffmpeg-audio/build.sh` makes from
FFmpeg's source, and puts its DLLs next to the built binaries; set
`NOVA_MEDIA_NATIVE_DIR=<dir>` to link `<dir>/ffmpeg-master-latest-win64-lgpl-shared/` instead
(same ABI, avcodec-62). Linux needs `ffmpeg`, `ffprobe` and PulseAudio at run time.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Player-Core's own code is licensed under either of [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option. What a build links keeps its own terms: FFmpeg (LGPL),
`vendor/cpal` (Apache-2.0), the TAK decoder (non-commercial), and, with the `asio` feature,
Steinberg's ASIO SDK (GPL-3.0, or Steinberg's own licence; a binary built with `asio` follows
one of the two). The TAK decoder's licence restricts distribution: check it before shipping.
