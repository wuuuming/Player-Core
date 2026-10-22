/* The transport options one open carries and the URL-backed positional byte source read
 * through FFmpeg's own protocol layer. */
#include "player_core_internal.h"

/* Builds the open-time AVDictionary. An absent option is an absent key, never an empty string:
 * FFmpeg treats `user_agent=""` as an override to nothing, which is not the same as the default. */
int bn_http_open_options(AVDictionary **options,
                         const BnHttpOpenOptions *http) {
    if (http && http->headers && http->headers[0]) {
        const int result = av_dict_set(options, "headers", http->headers, 0);
        if (result < 0)
            return result;
    }
    if (http && http->user_agent && http->user_agent[0]) {
        const int result =
            av_dict_set(options, "user_agent", http->user_agent, 0);
        if (result < 0)
            return result;
    }
    /* Transport ceilings and reconnection. Without rw_timeout a half-open CDN connection parks the
     * decode thread with nothing to wake it. */
    static const char *const kHttpDefaults[][2] = {
        /* microseconds: the read/write ceiling the protocol layer enforces itself */
        {"rw_timeout", "10000000"},
        {"reconnect", "1"},
        {"reconnect_streamed", "1"},
        {"reconnect_delay_max", "4"},
        /* FFmpeg's own defaults retry without a count, for up to 256 s of delay in total. A worker
         * blocked in that loop outlives the session that asked. A source that fails this many reconnects is reopened instead. */
        {"reconnect_max_retries", "3"},
        {"reconnect_delay_total_max", "8"},
        /* NO persistent connection (`multiple_requests`). Kept for reuse, a connection has to be
         * read DRY before it can carry the next request, so a seek waited for the rest of the
         * response it was leaving: the whole rest of a long stream asked for "from here to the end",
         * minutes on 加载中 with the network flat out. Asking in slices
         * (`request_size`) only bounded that by the slice, and the bound is paid at the STREAM's
         * delivery rate — a CDN that paces sound near its bitrate made a 4 MiB slice a
         * seventeen-second seek where a fresh connection takes half a second. A seek that
         * closes its connection and opens another costs one handshake, the same on every link
         * and for every stream, and a connection the CDN dropped behind our back is never reused
         * either. */
        {"multiple_requests", "0"},
        /* NOT sliced on top of that, though slices fill the read-ahead faster (a CDN bursts the
         * start of every response, then paces it). Measured: slices on kept connections
         * are what drained for seventeen seconds; slices with a connection each made a session's
         * first read fail in two runs of the laws out of three. One open-ended response per
         * position, dropped whole on a seek, is the combination that held every time. */
    };
    for (size_t i = 0; i < sizeof(kHttpDefaults) / sizeof(kHttpDefaults[0]); ++i) {
        /* AV_DICT_DONT_OVERWRITE: a caller-supplied value stays the caller's. */
        const int result = av_dict_set(options, kHttpDefaults[i][0],
                                       kHttpDefaults[i][1], AV_DICT_DONT_OVERWRITE);
        if (result < 0)
            return result;
    }
    return 0;
}

/* No async: wrapper: the decoder already owns a read-ahead thread. A second protocol thread
 * races operation deadlines and reads while the source is idle. Direct I/O has one owner.
 * UI threads never perform these calls. */
int32_t bn_url_source_open_with_error(const char *url, const BnHttpOpenOptions *http,
                           int (*cancelled)(void *), void *cancel_opaque,
                           BnUrlSource **out_source, char *error,
                           size_t error_capacity, int32_t *native_error) {
    if (native_error) *native_error = 0;
    if (!url || !out_source) {
        bn_error(error, error_capacity,
                 "url source open received an invalid argument");
        return BN_OPEN_INVALID_ARGUMENT;
    }
    *out_source = NULL;

    /* Allocated before the open so the interrupt deadline the context keeps a pointer to outlives
     * this call, and is still valid when the close path calls the callback one last time. */
    BnUrlSource *source = av_mallocz(sizeof(*source));
    if (!source) {
        bn_error(error, error_capacity,
                 "could not allocate the url source");
        return BN_OPEN_DECODER_ALLOCATION;
    }
    source->interrupt.external_cancelled = cancelled;
    source->interrupt.cancel_opaque = cancel_opaque;
    bn_interrupt_deadline(&source->interrupt, BN_OPEN_DEADLINE_US);

    AVDictionary *options = NULL;
    int result = bn_http_open_options(&options, http);
    AVIOContext *io = NULL;
    AVIOInterruptCB interrupt_cb = {bn_interrupt_expired, &source->interrupt};
    if (bn_interrupt_expired(&source->interrupt))
        result = AVERROR_EXIT;
    if (result >= 0)
        result = avio_open2(&io, url, AVIO_FLAG_READ, &interrupt_cb, &options);
    av_dict_free(&options);
    if (result < 0) {
        if (native_error) *native_error = result;
        bn_av_error(error, error_capacity, "could not open the media url",
                    result);
        av_freep(&source);
        return BN_OPEN_INPUT;
    }

    source->io = io;
    source->size = avio_size(io);
    if (bn_interrupt_expired(&source->interrupt)) {
        if (native_error) *native_error = AVERROR_EXIT;
        bn_error(error, error_capacity, "media open cancelled or timed out");
        bn_url_source_destroy(&source);
        return BN_OPEN_INPUT;
    }
    bn_interrupt_deadline(&source->interrupt, 0);
    *out_source = source;
    return BN_OPEN_OK;
}

void bn_url_source_destroy(BnUrlSource **source) {
    if (!source || !*source)
        return;
    avio_closep(&(*source)->io);
    av_freep(source);
}

int64_t bn_url_source_size(const BnUrlSource *source) {
    return source ? source->size : (int64_t) BN_OPEN_INVALID_ARGUMENT;
}

int32_t bn_url_source_read(BnUrlSource *source, int64_t offset,
                           uint8_t *buffer, int32_t size, int64_t budget_us) {
    if (!source || !source->io || !buffer || size <= 0 || offset < 0)
        return BN_OPEN_INVALID_ARGUMENT;
    /* One budget includes seek, response headers, read and every reconnect. Reset BEFORE
     * seeking: an earlier read's deadline must not reject a valid later seek. */
    if (budget_us <= 0)
        return AVERROR_EXIT;
    bn_interrupt_deadline(&source->interrupt, FFMIN(budget_us, BN_READ_DEADLINE_US));
    if (bn_interrupt_expired(&source->interrupt))
        return AVERROR_EXIT;
    /* At or past the end of a resource whose size the transport declared, the answer is
     * end-of-input by definition — the caller's `read` contract says so. Asking the network
     * anyway earns a 416 for a question already answered. */
    if (source->size >= 0 && offset >= source->size)
        return 0;
    const int64_t seek = avio_seek(source->io, offset, SEEK_SET);
    if (bn_interrupt_expired(&source->interrupt))
        return AVERROR_EXIT;
    if (seek < 0)
        return (int32_t) seek;
    /* Whatever has arrived, not the whole request: `avio_read` holds out for all `size` bytes, and
     * the caller asks for a 64 KiB block — seven seconds of this sound on a link that delivers it
     * in real time, all of which the decoder then waited for before it saw its first packet. */
    const int read = avio_read_partial(source->io, buffer, size);
    if (bn_interrupt_expired(&source->interrupt))
        return AVERROR_EXIT;
    if (read == AVERROR_EOF || read == 0) {
        if (source->io->error < 0 && source->io->error != AVERROR_EOF)
            return source->io->error;
        /* Content-Length promised bytes that never arrived: not successful EOF. */
        if (source->size >= 0 && offset < source->size)
            return AVERROR(EIO);
        return 0;
    }
    return (int32_t) read;
}
