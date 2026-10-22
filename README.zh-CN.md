<h1 align="center">Player-Core</h1>

<p align="center"><b>NovaPlayer 背后的音频播放内核，用 Rust 编写。</b></p>

<p align="center">
  <a href="#许可证"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue" alt="许可证"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux-lightgrey" alt="平台">
</p>

<p align="center"><a href="README.md">English</a> | 简体中文</p>

Player-Core 是 [NovaPlayer](https://github.com/wuuuming/NovaPlayer) 里负责出声的部分。它打开歌曲、解码，
再播放到设备上：Windows 上通过 FFmpeg 和 WASAPI 或 ASIO，Linux 上通过系统的 FFmpeg 和 PulseAudio（或
PipeWire 的 pulse 服务）。它自己没有界面，目前只有 NovaPlayer 一个宿主，但里面没有任何地方依赖 NovaPlayer。

它做的事：

- 解码 FFmpeg 能解码的一切，外加 TAK；DSD 经 ASIO 原生输出或以 DoP 输出。读取 CUE，以及旧代码页编码的
  标签和歌词。
- 无缝播放：下一首提前打开，接在同一条流里，精确到采样。需要时做等功率的淡入淡出。
- 跟着硬件走：播放跟随 Windows 的默认输出设备，选定的设备重新插上后会切回去。
- 两个平台同一条处理链：带峰值限制器的回放增益、进程内加载的 CLAP 效果器，以及逐帧平滑、不会产生
  拉链噪声的音量。细节见 [docs/processing.md](docs/processing.md)（英文）。
- 读取本地文件、HTTP(S)，或宿主实现的任何字节源（`media-io`，不依赖 FFmpeg 也能构建）。

各部分在哪里见 [docs/map.md](docs/map.md)（英文）。

## Crate

| Crate | |
|---|---|
| `player-core` | 内核：探测、解码、输出、处理 |
| `media-io` | 字节源，以及宿主要实现的音频源工厂；不依赖 FFmpeg |

## 构建

```sh
cargo build --release                              # Windows 或 Linux，不需要事先准备任何东西
cargo build --release --features asio              # Windows，带 ASIO（需要 LLVM 的 libclang）
python tools/ci.py                                 # CI 跑的内容：fmt、构建、clippy、单元测试
```

Windows 上构建时链接仓库里的纯音频 FFmpeg（`crates/player-core/third_party/ffmpeg/`，FFmpeg c6bb22dea0，LGPL，五个 DLL，约 9 MB，由 `tools/ffmpeg-audio/build.sh` 从 FFmpeg 源码构建），并把 DLL 放到构建出的程序旁边；设置 `NOVA_MEDIA_NATIVE_DIR=<dir>` 可改用 `<dir>/ffmpeg-master-latest-win64-lgpl-shared/`（同一 ABI，avcodec-62）。Linux 运行时需要 `ffmpeg`、`ffprobe` 和 PulseAudio。

## 参与贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

Player-Core 自己的代码以 [Apache-2.0](LICENSE-APACHE) 或 [MIT](LICENSE-MIT) 授权，任选其一。构建链接的组件保留各自的条款：FFmpeg（LGPL）、`vendor/cpal`（Apache-2.0）、TAK 解码器（非商业使用），以及启用 `asio` 特性时的 Steinberg ASIO SDK（GPL-3.0）。（或 Steinberg 自己的许可，带 `asio` 构建的二进制遵循其一）。TAK 解码器的许可限制分发，发布前请先确认。

如中英文版本有出入，以英文版为准。
