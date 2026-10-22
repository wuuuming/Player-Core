# Sound processing

The same chains on both platforms (`src/dsp.rs`): PCM runs through `DspStage`s (`process`,
`reset`, `latency`) in blocks of at most `DSP_MAX_BLOCK_FRAMES`, reset on seek, a stage's latency
taken out of the timeline and its tail flushed at the end. On Windows the decode worker runs them
between the decoder and the ring (DSD bypasses them; the stream's chain sits with the ring, in
`audio/stream.rs`); on Linux the Pulse writer runs them between FFmpeg's pipe (48 kHz stereo
`f32le`) and the `float32le` stream.

Two chains, by what they belong to. A song's own (`song_chain`) is its ReplayGain
(`AudioOpenOptions::replay_gain`, `AudioPlayer::set_replay_gain`): track or album gain from the
tags, changed from the next block processed with a one-pole glide, and a peak limiter for gain
above 0 dB. The stream's (`stream_chain`) is the host's CLAP effects (`AudioOpenOptions::effects`),
which every song of the stream goes through one after another. The user's volume is a factor
smoothed per frame: in the device callback on Windows, after the chains on Linux.


## Gapless

`AudioPlayer::set_next` names the song to go on into when this one ends (`AudioNext`: its source,
its length, a CUE span). It is opened ahead and continues the same stream at the sample after
this one's last: on Windows a second decoder (thread `player-core-audio-next`) is handed to the
decode worker, which switches to it in the same ring at the first song's end or stop bound; on
Linux the writer starts the next FFmpeg child ahead and splices its output in at the first one's
EOF. FFmpeg trims each codec's delay and padding (LAME header, MP4 edit list), so the joint is
sample-exact (checked in the private test suite against generated fixtures). The next
song has its own ReplayGain; the effects go on across the boundary as within a song, not reset
there, so a reverb's tail runs on into the next song. Only an effects list changed since
(an effect added, removed or moved) renews them at the boundary: the first song's tail is flushed
out of them, then they are reset and take up the change. The volume carries on.

## Crossfade

`AudioNext::crossfade` overlaps the next song with the end of this one: the same path as gapless,
with an overlap of more than nothing. The overlap ends where this song ends as known ahead — its
stop, else the length the host gave — so a song whose end is not known, and a DSD one, is followed
gaplessly. At the overlap's start the going song's decoder plays on beside the coming one's: on
Windows into a ring of its own (`StreamWriter::begin_fade`), on Linux from its FFmpeg child read
alongside. Each goes through its own chain (its own ReplayGain), the two are summed under
equal-power gains (`dsp::Crossfade`: cos and sin of a quarter turn over the span), and the sum
goes on through the stream's chain and the volume. The coming song is taken only as far as the
going one has given, so neither decoder waits on the other's ring. A going song that ends early
(or fails) leaves the coming one to finish rising alone; a coming one shorter than the span ends
it. The host hears of the next song at the overlap's start, its first frame, as without one: from
there it is the song playing, while the going one fades out under it.

`AudioPlayer::followed` reports the `AudioNext::id` once its first sample (with a crossfade, the
overlap's start) is heard; from then on
position, duration, stop (and on Windows the stream info) are the new song's. A seek back before
the boundary is heard stays in the first song, which goes on into the next again at its end. A
song that cannot follow in the same stream (not ready in time, DSD, a DSD output, another rate
on ASIO) is not followed: the first one ends as before and the host switches the ordinary way.
