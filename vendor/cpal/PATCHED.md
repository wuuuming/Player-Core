# cpal 0.15.3, patched for Player-Core

The crates.io release, unchanged except for `src/host/wasapi/stream.rs`
`process_commands_and_await_signal`: while the stream is paused, the run thread waits on the
command event only, not on the WASAPI event as well.

Why: Windows goes on signalling a shared-mode event-driven stream's event every period
(~10 ms) after `IAudioClient::Stop`, and after `Reset` (Wine's `dlls/mmdevapi/tests/render.c`
asserts it: "Still receiving events!"). Upstream cpal waits on that event regardless, so a paused
stream's `cpal_wasapi_out` thread woke ~100 times a second for as long as it stayed paused.

Also `src/host/wasapi/device.rs` `ensure_future_audio_client` names the guard's elided
lifetime (`MutexGuard<'_, …>`): as a path dependency the crate's warnings are no longer capped.

Wired by `[patch.crates-io]` in Player-Core's workspace `Cargo.toml` and in NovaPlayer's (git,
the same Player-Core rev). Drop it when upgrading to a cpal release that stops waiting on a
stopped stream's event.
