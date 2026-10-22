/* Shared across the authored shim's translation units. Not a public surface: `player_core.h`
 * is the ABI the Rust side binds, and nothing here changes it. What lives in this file is
 * what more than one .c file needs - the includes they all take, the deadline and queue
 * dimensions, the handle definitions, and the prototypes of the helpers more than one file
 * calls.
 */
#ifndef BN_PLAYER_CORE_INTERNAL_H
#define BN_PLAYER_CORE_INTERNAL_H

#include "player_core.h"

#include <errno.h>
#include <math.h>
#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/dict.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/time.h>
#include <libavutil/macros.h>
#include <libavutil/mathematics.h>
#include <libavutil/mem.h>
#include <libswresample/swresample.h>

#define BN_AVIO_BUFFER_SIZE (64 * 1024)

typedef struct BnInterrupt {
    /* `av_gettime_relative()` microseconds, or 0 for "no deadline". */
    volatile LONG64 deadline_us; /* Access ONLY through Interlocked operations. */
    volatile LONG cancelled;
    /* Immutable after publication. Owner retains opaque until all native calls finish. */
    int (*external_cancelled)(void *);
    void *cancel_opaque;
} BnInterrupt;

/* Wall-clock budget for each blocking operation, including protocol reconnects. */
#define BN_OPEN_DEADLINE_US (15 * 1000 * 1000)
#define BN_READ_DEADLINE_US (10 * 1000 * 1000)

/* The container is read on its own thread.
 *
 * Reading a network source can block for as long as the network likes: a CDN connection that
 * went quiet, FFmpeg's http layer reconnecting. The demux thread owns every call into the
 * AVFormatContext once the decoder is open: it reads the audio stream's packets into a bounded
 * queue and performs seeks. The decode worker takes packets from the queue and waits at most
 * `BN_AUDIO_TAKE_WAIT_MS`: an empty queue is "no packet yet", which the player treats as
 * buffering. A seek is a request: the queue is flushed at once and the packets after the
 * reposition carry the new epoch.
 *
 * How far ahead it reads depends on where the bytes come from (`bn_demux_size`). A network source
 * is read up to `BN_AUDIO_DEMUX_BYTES_LIMIT` ahead, minutes of compressed sound, so jitter and a
 * stall are ridden out and a seek into what is already here costs no request. A local file
 * refills from disk in milliseconds; what it keeps queued is resident memory and buys nothing, so
 * it is read about 2 s ahead and a seek re-reads the disk. */
#define BN_DEMUX_QUEUE_PACKETS_LIMIT 8192
#define BN_READ_AHEAD_LOCAL_US ((int64_t) 2 * 1000 * 1000)

typedef struct BnDemux {
    /* What it reads — bound by its owner before first use, never changed after. */
    AVFormatContext *format;
    int stream_index;
    /* The owner's interrupt cell: an expired deadline aborts whatever FFmpeg wait is in flight. */
    BnInterrupt *interrupt;
    /* The source is a local file (`bn_*_decoder_set_source_local`); a source nobody described is
     * read as a network one. */
    int source_local;
    /* When the queue is full, set by `bn_demux_size`: at `bytes_limit` bytes, or once it spans
     * `horizon_us` (0: no time rule) and holds at least `bytes_floor`. */
    size_t bytes_limit;
    size_t bytes_floor;
    int64_t horizon_us;
    HANDLE thread;
    SRWLOCK lock;
    CONDITION_VARIABLE changed;
    AVPacket **ring;
    int capacity;
    int head;
    int count;
    size_t bytes;
    /* Bumped by every seek request; a packet read under an older epoch is discarded. */
    uint64_t epoch;
    int seek_pending;
    int64_t seek_min;
    int64_t seek_target;
    int64_t seek_max;
    int seek_flags;
    /* The container ended, or failed, for the current epoch. */
    int eof;
    int error;
    int stop;
    /* The newest queued packet's pts, for how much is buffered. */
    int64_t newest_pts;
    /* Transparent recovery from a transport error (see `bn_demux_recover`). `last_dts` is the last
     * packet READ from the container this epoch; after a recovery reposition, packets up to
     * `skip_through_dts` were already queued and are dropped. `retries` counts recoveries since
     * the last packet that arrived. */
    int64_t last_dts;
    int64_t skip_through_dts;
    int retries;
    int has_seek_target;
} BnDemux;

typedef struct BnTakDecoder BnTakDecoder;

struct BnAudioDecoder {
    /* Last failed transport operation, owned by the decoder caller (not the demux thread). */
    int32_t transport_error;
    BnTakDecoder *tak;
    AVCodecParserContext *flac_parser;
    int flac_block_size;
    int input_sample_rate;
    AVFormatContext *format;
    AVIOContext *custom_io;
    AVCodecContext *codec;
    AVPacket *packet;
    AVFrame *frame;
    SwrContext *resampler;
    AVChannelLayout output_layout;
    float *output_samples;
    int output_capacity_frames;
    int audio_index;
    int output_sample_rate;
    int output_channels;
    int input_eof;
    int resampler_drained;
    int has_next_output_sample_pts;
    int64_t next_output_sample_pts;
    /* Source frames handed to the resampler since the decoder opened: with the frames it gave
     * back, what a diagnostic compares to see that nothing was dropped or repeated. */
    int64_t resampler_input_frames;
    /* Codec frames that did not decode, passed over since the decoder opened. */
    int64_t passed_over;
    /* This run — from the open, or the last seek — has passed a frame over: FFmpeg's stamps after
     * it are not trusted to go forward, and the output continues where it ended instead. */
    int run_passed_over;
    /* A frame was passed over since the last one that came out: the next frame with a timestamp
     * measures what went missing against where the last output ended. */
    int gap_open;
    /* A seek has no previous output block. The first bad packet supplies a separate start for
     * its missing span, bounded by the requested position; the pump trims any preroll. */
    int has_seek_gap_base;
    int64_t seek_target_sample_pts;
    int64_t seek_gap_base_sample_pts;
    int has_packet_sample_pts;
    int64_t packet_sample_pts;
    /* Output frames of silence still owed for what was passed over. `frame` is the frame that
     * measured it, held — its place, `held_output_pts`, already taken from the resampler — until
     * the silence before it is paid. */
    int64_t silence_owed;
    int frame_held;
    int64_t held_output_pts;
    /* Raw DSD bypass. The normal codec and resampler may have been opened while probing, but
     * this mode consumes demux packets directly and never sends them to either. */
    int dsd_enabled;
    int dsd_decode_started;
    int dsd_lsbf_planar;
    int dsd_channels;
    int dsd_byte_rate;
    int dsd_has_position;
    int64_t dsd_emitted_bytes;
    int64_t dsd_total_bytes;
    uint8_t *dsd_bytes;
    int dsd_capacity;
    const char *codec_name;
    /* The container is read on a demux thread of its own: the decode worker takes packets from a
     * queue that keeps filling while the transport is paused or buffering, and waits for one in
     * short slices so it can still hear its commands. */
    BnDemux demux;
    BnInterrupt interrupt;
};

int bn_tak_open(BnAudioDecoder *decoder, const wchar_t *path, const BnByteSource *source,
                AVChannelLayout *layout, char *error, size_t capacity);
void bn_tak_destroy(BnTakDecoder *decoder);
int bn_tak_receive(BnAudioDecoder *decoder, char *error, size_t capacity);
int bn_tak_seek(BnAudioDecoder *decoder, int64_t pts, char *error, size_t capacity);

/* Compressed audio is small: this is minutes of it, and what bounds a network source's prefetch,
 * with no time rule. A local file's queue is at least the floor: at a low bitrate 2 s is a few
 * kilobytes, and reads that small cost more in wake-ups than they save. */
#define BN_AUDIO_DEMUX_BYTES_LIMIT ((size_t) 8 * 1024 * 1024)
#define BN_AUDIO_READ_AHEAD_FLOOR_BYTES ((size_t) 256 * 1024)
/* How long the decode worker waits for a packet before it looks at its commands again. */
#define BN_AUDIO_TAKE_WAIT_MS 50

/* A network resource read POSITIONALLY, through the same protocol layer and the same admission
 * options a decoder open takes: the player's sources are byte-addressed, and FFmpeg's HTTP layer
 * is the one HTTP client this crate links. */
struct BnUrlSource {
    AVIOContext *io;
    /* Negative when the transport never declared one. */
    int64_t size;
    /* The open deadline the protocol layer polls. It lives HERE, with the source, because
     * `avio_open2` keeps the callback's `opaque` pointer inside the AVIOContext and calls it
     * again on close: a deadline held in the opening frame's locals is a dangling stack pointer
     * by then. */
    BnInterrupt interrupt;
};

/* Helpers that were `static` while this was one file and are now called across two. Nothing
 * here is new code: each is the same function, with `static` dropped at its definition. */
int bn_interrupt_expired(void *opaque);
void bn_interrupt_deadline(BnInterrupt *interrupt, int64_t budget_us);
int bn_source_read(const BnByteSource *source, BnInterrupt *interrupt,
                   uint8_t *buffer, int size);
void bn_error(char *error, size_t capacity, const char *format, ...);
void bn_av_error(char *error, size_t capacity, const char *operation, int code);
void bn_custom_io_free(AVIOContext **custom_io);
void bn_copy_text(char *destination, size_t capacity, const char *source);
int bn_custom_input_open(AVFormatContext **format, AVIOContext **custom_io,
                         const BnByteSource *source, BnInterrupt *interrupt);

/* bn_demux.c */
int64_t bn_demux_queued_us_locked(const BnDemux *demux);
int bn_demux_covers_locked(const BnDemux *demux, int64_t target);
int bn_demux_trim_to_locked(BnDemux *demux, int64_t target);
int bn_demux_ensure(BnDemux *demux);
void bn_demux_size(BnDemux *demux);
void bn_demux_set_source_local(BnDemux *demux, int local);
int bn_demux_take(BnDemux *demux, AVPacket *out);
int bn_demux_request_seek(BnDemux *demux, int64_t min_ts, int64_t target,
                          int64_t max_ts, int flags);
void bn_demux_stop(BnDemux *demux);
void bn_demux_cancel(BnDemux *demux);

/* bn_http.c */
int bn_http_open_options(AVDictionary **options, const BnHttpOpenOptions *http);

#endif /* BN_PLAYER_CORE_INTERNAL_H */
