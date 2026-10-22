# Player-Core map

What exists and where. It is facts only. A change that
adds, moves or removes a module or a mechanism updates this file in the same commit.

What the sound does (the chain, gapless, ReplayGain) is in [`processing.md`](processing.md); how
to build and what a build links is in the [README](../README.md).

## 1. Layers

```
host (NovaPlayer)                       one AudioPlayer per song; wake callback + service()
 └─ AudioPlayer                         same public API on both platforms (api_parity.rs)
     ├─ Windows  src/audio/             in-process FFmpeg (C shim) → ring → WASAPI / ASIO
     │   media loop (host thread) ── AudioOutput ── orders ──▶ owner thread ── binding
     │                                    ◀── edges ──          ├─ decode worker: pump → DSP chain → ring
     │                                                          └─ device callback: ring → volume → device
     └─ Linux    src/linux/             ffmpeg CLI child → pipe → writer thread (DSP chain, volume) → PulseAudio
shared       dsp.rs, replay_gain.rs, media_tags.rs, tag_rule.rs, timing.rs, clap/ (CLAP effects), crates/media-io (sources)
native       src/native/*.c (Windows only): decoder, demux thread, URL source, tags, TAK, upmix, ASIO
```

Threads, Windows: the host's media loop (calls `AudioPlayer`); `player-core-audio-owner` (owns the
binding, the device stream and every rebind); `player-core-audio-decode` (one per binding: decodes,
runs the chain, fills the ring; MMCSS "Audio"); `player-core-audio-next` (opens the next song);
the device callback (MMCSS "Pro Audio"); one demux thread per decoder (C); the endpoint watcher;
`clap-host`. Linux: the host; `pulse-out` (one per run: drives the ffmpeg child, runs the chain,
writes Pulse); `audio-next` (probes the next song); the feeder and stderr threads of each child;
`clap-host`.

## 2. Flows

**Open → play (Windows).** `AudioPlayer::open_with` → `AudioOutput::open_hinted` (retries a held
ASIO driver, falls back to WASAPI) → `open_once`: `AudioSource` from the factory, `AudioSession`,
`prepare_binding` on the caller (fails fast), `OutputAuthorityLease`, spawn the owner thread,
which commits the binding (`spawn_decode_worker`, device stream) and posts `AudioEdge::Landed`.
`play`/`pause` publish a posture (`publish_posture`); the owner applies it as ramp + hardware step
(`posture.rs`).

**Sound path (Windows).** C decoder (`bn_audio_decoder_decode_next`, packets from the demux
thread) → `DecoderPump::decode_block` (timestamps normalised, preroll trimmed, stop bound) →
`MixStage` when the output has ≥4 channels (`mix.rs`) → the song's `DspChain` (`dsp.rs`: ReplayGain) → the stream's (effects, `stream.rs`)
→ `AudioRing` → `CallbackCore::render` (ramp, user volume, delivery published to `AudioClock`) →
device. DSD: bytes bypass mixer and chain (`dsd.rs`, `asio_writer.rs`).

**Seek (Windows).** `begin_seek_hold` (held posture) → `seek_held_posted` (new generation,
`AudioOrder::Seek`) → owner → `WorkerCommand::Seek` → pump seek, ring discarded, re-anchor →
`finish_seek`; the position promised until the clock adopts the generation (`seek.rs`).

**Device loss (Windows).** cpal error / endpoint events → `AudioDevicePhase` ladder (`rebind.rs`)
→ `AudioOrder::Attempt` → owner rebinds at the promised position (`binding.rs` `rebind`).

**Gapless (Windows).** `set_next` → `open_next` → `follow::spawn_next` (decoder, own chain) →
`WorkerCommand::Next` → worker swaps pumps at the end/stop bound, or at the handover before the
song's known end for a crossfade (the old pump then refills `StreamWriter`'s going ring until the
overlap is over), keeps the old one 2 s for a seek back → `AudioEdge::Followed` (the overlap's
start) → `take_up_boundaries` when the clock reaches it → `followed()`.

**Linux.** `start()` spawns `pulse-out` running `write_run`: ffmpeg child (`-ss`/`-t` for the
span) → `Processing::block` (chain, volume) → Pulse stream. Play/pause/seek halt the run and start
another at the position. Gapless: the next song's child starts ahead (`Ahead`) and is spliced in
at EOF (`Run::cross`), heard when Pulse's latency says so (`take_up`). Crossfade: the read stops at
the handover (`Writer::read_end`), the going child becomes `Going` and is read alongside
(`read_going`), `Processing` sums the two.

## 3. Modules

### Shared (`src/`)
| Module | Owns |
|---|---|
| `lib.rs` | public re-exports per platform; `instrument` counters |
| `internals.rs`, `dsd_files.rs` | `test-support` only, hidden: what the private test suite reaches that is not API (the pump over a file, `audio/test_pump.rs`; the ring, clock, DSP chain, CLAP library, Linux processing); generated DSF/DFF files (also this crate's unit tests) |
| `dsp.rs` | `DspStage`, `DspChain` (latency, lead-in drop, flush, `renew` at a boundary), `song_chain` / `stream_chain`, `Crossfade` (equal-power sum of two songs), `one_pole`, `PeakLimiter` (ceiling and look-ahead given; ReplayGain's and the speakers'), `ReplayGainSetting` (shared atomic), `ReplayGainStage` |
| `replay_gain.rs` | `ReplayGain` mode; `ReplayGainTags` (REPLAYGAIN_* / R128_*), factor with fallback |
| `media_tags.rs` | `MediaTags`, `AudioResolution` (one type for both platforms; `AudioResolution::of`: a resolution from what a reader found, `label`) |
| `tag_rule.rs` | what a file says, one rule for both platforms: `Said` (its tags by FFmpeg's names) and `Said::tags` (the words of a `MediaTags`), each field's names, lyrics, lossless codecs, the source's depth |
| `timing.rs` | `NormalizedTimestamp`, `StreamTiming` (raw ↔ media time) |
| `legacy_text.rs` / `linux/legacy_text.rs` | legacy code-page text: tags, CUE, m3u |
| `clap/mod.rs` | `Effects` (shared chain of plugin instances: add (or `add_then`, not waiting; a stage waits for pending adds), remove, move, bypass, params, state, windows), `describe`, `find_plugins` |
| `clap/stage.rs` | `EffectsStage`: a `DspStage` over the shared instances |
| `clap/instance.rs` | one plugin: create, activate, process (try-lock; failed → out for life), params, state, GUI, host extensions |
| `clap/host.rs` | the `clap-host` thread (CLAP main thread): jobs, timers, fds, message pump |
| `clap/library.rs` | loading a `.clap` (one load per path), descriptions |
| `clap/frame.rs` | the host's own window for an embedded plugin GUI (Win32, X11) |
| `crates/media-io` | `MediaByteSource`, `SourceLocality`, `FileMediaByteSource`, `AudioSourceFactory`, `PathAudioSourceFactory`, `AudioSourceError`; `input.rs`: `AudioInput`, `HttpOpenOptions`, `PcmSource`, `PcmAudioSourceFactory` (host PCM as a WAV file) |

### Windows (`src/audio/`, `src/source/`, `src/native/`)
| Module | Owns |
|---|---|
| `player.rs` | `AudioPlayer`, `AudioOpenOptions`, `AudioNext` |
| `output.rs` | `AudioOutput`: the media loop's handle: posture, seek transaction, device ladder, edges, next song, boundaries, positions |
| `session.rs` | `AudioSession` (what outlives bindings), `StopBound`, `AudioOrder`, `AudioEdge`, `Followed`, `AudioSourceState` |
| `owner.rs` | the owner thread: applies orders, posture, rebinds |
| `binding.rs` | `prepare_binding` / `commit_binding` / rebind: source + decoder + device + worker + stream; `decoded_track` |
| `worker.rs` | `DecodeWorker`, one method per job on each pass: commands, seek (and back over a boundary), anchor, gapless boundary, previous song kept, refill, posting the boundary, park |
| `follow.rs` | `NextTrack`, `can_follow`, `open_next`, `spawn_next` |
| `pump.rs` | `DecoderPump`: decoder → (mixer) → chain → ring; the song's end (`SongEnd`: file or stop bound); anchor; `open_native_audio` |
| `callback.rs` | `CallbackCore` (device side: ramps, volume, delivery, supply), `CallbackControl`, `UserGain` |
| `ring.rs` | `AudioRing` (SPSC f32 / DSD bytes) |
| `stream.rs` | `StreamWriter`: the worker's side of the ring, the stream's chain before it, frames given since the generation started (a boundary's place), a crossfade's going ring and sum |
| `clock.rs` | `AudioClock` (anchor + delivery seqlocks), `GenerationCounter`, `PlaybackRateControl` |
| `posture.rs`, `park.rs`, `seek.rs` | transport posture law, park state, seek promise |
| `rebind.rs` | device-loss ladder `AudioDevicePhase`, backoff, `StreamFaultSink` |
| `device.rs` | output systems, route state (`RouteState`), endpoint watch hooks, null output, device/stream resolve and build |
| `endpoints.rs` | WASAPI endpoints (COM): layouts, default, cpal pairing, notification thread |
| `asio_host.rs`, `asio.rs`, `asio_writer.rs`, `bn_asio.cpp` | ASIO: driver thread, native DSD and DoP output and their writers (PCM on ASIO goes through cpal) |
| `dsd.rs` | `DsdCallback`: DSD bytes to the device |
| `mix.rs`, `mix_engine.rs`, `bn_upmix.c` | fixed channel mixing (route, downmix, upmix) with the speakers' limiter, upmix filter graph |
| `track.rs`, `tags.rs` | `AudioStreamInfo`, track enumeration; `probe_media_tags` |
| `policy.rs`, `error.rs`, `metrics.rs`, `lease.rs`, `mmcss.rs`, `ffi.rs` | buffer policy, errors, counters, device ownership lease, thread priority, C ABI |
| `source/` | `UrlAudioSourceFactory` (FFmpeg's HTTP), `NativeMediaByteSource` (custom AVIO), native errors |
| `native/bn_audio_decoder.c` | decoder: open, seek, decode (resample, timing, passed-over frames as silence), DSD bytes |
| `native/bn_demux.c` | demux thread: packet queue, read-ahead, seek from queue, transport-error recovery |
| `native/bn_common.c`, `bn_http.c`, `bn_tags.c`, `bn_tak_decoder.c` | interrupts and custom AVIO; URL source; tags probe (every tag as the file holds it; `tag_rule.rs` reads them); TAK |

### Linux (`src/linux/`)
| Module | Owns |
|---|---|
| `player.rs` | `AudioPlayer`, `Run`, `Following`, `AudioNext`, `AudioOpenOptions` |
| `playback.rs` | `write_run` and its `Writer` (look ahead, route moves, read, wait, handover, song end), ffmpeg child, `Processing` (chains, crossfade sum, volume), `Ahead` (next song), `Going` (crossfade), errors from stderr |
| `pulse/` | libpulse by `dlopen`: `Stream` (open, write, latency, drain, flush, fade-out); `Recent`, the sound last written, kept for a fade-out |
| `output.rs` | output selection through `pactl`; null and capture outputs (tests) |
| `probe.rs` | tags and duration (lofty, else ffprobe JSON), ReplayGain tags for playback |
| `process.rs` | child ownership, bounded waits, stderr tail, `capture` |
| `source.rs`, `types.rs` | `UrlAudioSourceFactory` (the address to ffmpeg), the input's ffmpeg form; `AudioError`, `TrackHint` |

Linux runs the system's `ffmpeg`, `ffprobe` and `pactl` and loads `libpulse.so.0` at run time.
One `media-io` `AudioSourceFactory` serves both platforms: Windows reads its bytes (`open`),
Linux hands ffmpeg its `input` (a path, an address with its headers and agent, or host PCM fed on
stdin). `PathAudioSourceFactory`, `UrlAudioSourceFactory` and `PcmAudioSourceFactory` exist on
both; a byte-only factory (SFTP) plays on Windows only. `AudioPlayer` and `AudioOpenOptions` are
the same on both hosts, method for method (`src/api_parity.rs` reads the two sources and fails on
any difference not listed there). Linux plays FFmpeg's default stream (`TrackHint` where Windows
takes an `AudioTrackSelection`) and reports a failed open as an `AudioError`; it has no playback
rate, stream description, callback metrics or WASAPI timing policy. Real output tests need
`NOVA_PULSE_TEST=1` and a disposable null sink (`PULSE_SINK`), never the user's default device.

## 4. Mechanisms and the semantics they carry

A new semantic first looks here for the mechanism that can carry it (principles D2).

Scope is what a mechanism lives as long as; thread is where it runs. A new semantic goes where its
role puts it: a song's thing in a song's scope, the listener's in the stream's (principles D2).

| Mechanism | Where | Scope · thread | Semantics it carries | The part a semantic hands it |
|---|---|---|---|---|
| DSP chains | `dsp.rs` `DspStage`/`DspChain` | the song's: a song; the stream's: a binding (Win) or a run (Linux) · decode worker (Win), `pulse-out` (Linux) | ReplayGain and peak limiting (song); CLAP effects across boundaries (stream); latency compensation; tails at a song's and the sound's end | a stage |
| Peak limiter | `dsp.rs` `PeakLimiter` | its stage · its stage's thread | full scale after ReplayGain; −1 dBFS with 1.5 ms look-ahead after mixing to speakers | a ceiling, a look-ahead |
| Song sequence | Windows `worker.rs` + `follow.rs`; Linux `playback.rs` `Ahead` + `Run::cross` | the stream · decode worker / `pulse-out`; next song opened on its own thread | gapless or crossfaded next song, seek back over a boundary, CUE spans | an `AudioNext` |
| Song end | `pump.rs` `SongEnd`, `session.rs` `StopBound`; Linux ffmpeg `-t` | a song · decode worker | the file's end, a CUE track's end; whether the sound ends there or a song follows | a position |
| Generation | `clock.rs` `GenerationCounter` | the player · written by the media loop | seek, rebind, cancelled attempts, late answers discarded | a number |
| Orders / edges | `session.rs` `AudioOrder`, `AudioEdge` | the player · media loop ↔ owner thread | every media-loop → owner request, every owner → media-loop event | an enum case |
| Host wake | `metrics.rs` `SharedMetrics::flag`, `Announce`; `next_wake_at` | the player · any thread → host | end of song, buffering, input ended, boundary heard, device faults | a flag / a deadline |
| Route state | `device.rs` `RouteState` | the process · any | listener's output, driver fallback, endpoint gone and back | a versioned choice |
| Device ladder | `rebind.rs` `AudioDevicePhase` | the player · media loop | device lost, rebind attempts, backoff, give up | an event |
| Byte source | `media-io` `MediaByteSource`, `AudioSourceFactory`; `source/` | a decoder · demux thread | local file, HTTP, plugin bytes, reopen on rebind | a factory |
| Custom AVIO + interrupt | `bn_common.c` | a decoder · demux thread | any byte source into FFmpeg; every blocking call's deadline and cancel | a `BnByteSource` |
| Demux thread | `bn_demux.c` | a decoder · its own thread | read-ahead, seek served from memory, transport recovery, input state | a decoder |
| Plugin host thread | `clap/host.rs` | the process · `clap-host` | every CLAP main-thread call, timers, fds, plugin windows | a job |
| Callback core | `callback.rs` | a binding · device callback (real time) | transport ramps (a play, pause, seek: the policy's `ramp`; a player disposed while it sounds: its `leave`, as far as the ring reaches), user volume, delivery clock, supply metrics | a sample type |
| Fade-out on stop (Linux) | `linux/pulse/recent.rs` `Recent`; `Stream::fade_out`, `Capture::fade_out`; `playback.rs` `Writer::close` | a run · its writer thread | a pause or seek (`DECLICK`), a player disposed (`LEAVE`, `Run::leaving`): what the stream holds past the server's read index written again faded at its own index, then drained | a fade length |

## 5. Features → where → tests

Unit tests sit beside their code (`src/**/tests.rs`, `*_tests.rs`, inline modules). The
integration tests, the fixtures and the unit tests that need a fixture or the test CLAP effect
are maintained separately, named here by file.

| Feature | Where | Unit tests (here) | The suite |
|---|---|---|---|
| Open, play, pause, seek, end | `audio/player.rs`, `output.rs`, `worker.rs`; `linux/player.rs` | `audio/tests.rs`, `linux/player/tests.rs` | `audio_player.rs`, `decoder_pump.rs` |
| Declick, fade-out when left | `callback.rs`, `policy.rs`; `linux/playback.rs`, `linux/pulse/recent.rs` | | `gapless.rs` (`a_pause_ends_the_sound_without_a_click`, `a_song_left_while_it_sounds_fades_out`) |
| Same API on both platforms | both `player.rs` | `api_parity.rs` | |
| Gapless, crossfade | `worker.rs`, `follow.rs`, `stream.rs`, `dsp.rs` `Crossfade`; `linux/playback.rs` | `dsp/tests.rs` | `gapless.rs` |
| ReplayGain | `replay_gain.rs`, `dsp.rs` | `dsp/tests.rs`, `replay_gain.rs` inline | `decoder_pump.rs`, `linux_decode.rs` |
| CLAP effects | `clap/` | | `clap_host.rs` (with the test effect, `clap-test-gain`) |
| Formats, timestamps | `bn_audio_decoder.c`, `pump.rs`, `timing.rs` | | `sound_formats.rs`, `audio_decoder_timing.rs`, `timing_units.rs` |
| TAK | `bn_tak_decoder.c` | | `tak_decoder.rs`, `linux_decode.rs` |
| DSD (native, DoP, PCM) | `bn_audio_decoder.c`, `dsd.rs`, `asio*.rs` | `audio/dsd_delivery_tests.rs`, `audio/asio_dsd_tests.rs`, `audio/asio/stream_tests.rs` | `dsd_reader.rs` |
| Multichannel mixing | `mix.rs`, `bn_upmix.c` | `audio/multichannel_tests.rs` | |
| Network sources, recovery | `source/`, `bn_http.c`, `bn_demux.c` | | `audio_source_contract.rs`, `audio_supply.rs`, `netsim.rs`, `loopback_http_server.rs` |
| Supply (no starved output) | `callback.rs`, `metrics.rs` | | `audio_supply.rs` |
| Tags, cover, lyrics | `tag_rule.rs` (names per field, lyrics, lossless codecs, depth: both platforms); readers `tags.rs` + `bn_tags.c`, `linux/probe.rs` (ffprobe, lofty) | `media_tags/tests.rs` | `media_tags.rs`, `legacy_tags.rs`, `linux_decode.rs` |
| Legacy text | `legacy_text.rs` (both) | `legacy_text/tests.rs` | |
| Output selection, fallback | `device.rs`, `endpoints.rs`; `linux/output.rs` | `audio/tests.rs` | |
| Linux writer | `linux/playback.rs` | `support/linux_playback.rs` |
