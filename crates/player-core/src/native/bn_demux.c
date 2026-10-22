/* The demux thread and its packet queue: the only code that touches an open AVFormatContext
 * once a decoder is up, including the transport-error recovery. */
#include "player_core_internal.h"

#ifdef BN_INSTRUMENT
/* What every demux queue in the process holds: payload bytes, packets, and the bytes of the
 * buffers carrying the payload. See `BnBufferCensus`. */
static volatile LONG64 bn_census[3];

/* Counts `packet` into (`sign` 1) or out of (-1) the census. */
static void bn_demux_census(const BnDemux *demux, const AVPacket *packet, int sign) {
    (void) demux;
    InterlockedExchangeAdd64(&bn_census[0], sign * (int64_t) packet->size);
    InterlockedExchangeAdd64(&bn_census[1], sign);
    InterlockedExchangeAdd64(&bn_census[2],
                             sign * (int64_t) (packet->buf ? packet->buf->size : packet->size));
}

void bn_buffer_census(BnBufferCensus *out) {
    if (!out)
        return;
    out->audio_queue_bytes = InterlockedCompareExchange64(&bn_census[0], 0, 0);
    out->audio_queue_packets = InterlockedCompareExchange64(&bn_census[1], 0, 0);
    out->audio_queue_buffer_bytes = InterlockedCompareExchange64(&bn_census[2], 0, 0);
}
#else
#define bn_demux_census(demux, packet, sign) ((void) 0)
#endif

static void bn_demux_clear_locked(BnDemux *demux) {
    while (demux->count > 0) {
        AVPacket *packet = demux->ring[demux->head];
        demux->ring[demux->head] = NULL;
        demux->head = (demux->head + 1) % demux->capacity;
        demux->count -= 1;
        bn_demux_census(demux, packet, -1);
        av_packet_free(&packet);
    }
    demux->head = 0;
    demux->bytes = 0;
    demux->newest_pts = AV_NOPTS_VALUE;
}

int64_t bn_demux_queued_us_locked(const BnDemux *demux) {
    if (demux->count == 0 || demux->newest_pts == AV_NOPTS_VALUE)
        return 0;
    const AVPacket *oldest = demux->ring[demux->head];
    const int64_t oldest_pts = oldest->pts != AV_NOPTS_VALUE ? oldest->pts : oldest->dts;
    if (oldest_pts == AV_NOPTS_VALUE || demux->newest_pts <= oldest_pts)
        return 0;
    const AVRational time_base = demux->format->streams[demux->stream_index]->time_base;
    return av_rescale_q(demux->newest_pts - oldest_pts, time_base, (AVRational){1, 1000000});
}

static int bn_demux_full_locked(const BnDemux *demux) {
    return demux->count >= demux->capacity || demux->bytes >= demux->bytes_limit ||
           (demux->horizon_us > 0 && demux->bytes >= demux->bytes_floor &&
            bn_demux_queued_us_locked(demux) >= demux->horizon_us);
}

/* The bitrate of what the demux reads, in bits per second, or 0 when nothing says. Known once the
 * input is probed, which is before the queue is first sized. The container's total is the fallback:
 * it overstates one stream's, which errs toward the larger queue. */
static int64_t bn_demux_bitrate(const BnDemux *demux) {
    const AVStream *stream = demux->format->streams[demux->stream_index];
    if (stream->codecpar->bit_rate > 0)
        return stream->codecpar->bit_rate;
    return demux->format->bit_rate > 0 ? demux->format->bit_rate : 0;
}

/* `seconds_us` of the stream in bytes, clamped to [floor, cap]; `fallback` when the bitrate is
 * unknown. */
static size_t bn_demux_span_bytes(const BnDemux *demux, int64_t seconds_us, size_t floor,
                                  size_t cap, size_t fallback) {
    const int64_t bitrate = bn_demux_bitrate(demux);
    if (bitrate <= 0)
        return fallback;
    const int64_t bytes = av_rescale(bitrate / 8, seconds_us, 1000000);
    if (bytes <= (int64_t) floor)
        return floor;
    return bytes >= (int64_t) cap ? cap : (size_t) bytes;
}

/* Sizes the queue for its source. Called when the demux is bound, before its thread starts, and
 * again if the source is described after that.
 *
 * A network source keeps the byte bound and no time rule. A local one is full at 2 s queued (and
 * at least `BN_AUDIO_READ_AHEAD_FLOOR_BYTES`); the byte cap behind it is twice the horizon at the
 * probed bitrate, which bounds a stream whose timestamps cannot be measured.
 *
 * A local container's other streams are discarded before the thread starts: the reader then
 * skips their samples (in a video file, the whole picture) instead of reading them to throw them
 * away. Not on a network source, where skipping a sample can cost a new request. */
void bn_demux_size(BnDemux *demux) {
    if (!demux->thread) {
        for (unsigned index = 0; index < demux->format->nb_streams; ++index) {
            if ((int) index != demux->stream_index)
                demux->format->streams[index]->discard =
                    demux->source_local ? AVDISCARD_ALL : AVDISCARD_DEFAULT;
        }
    }
    if (!demux->source_local) {
        demux->horizon_us = 0;
        demux->bytes_floor = 0;
        demux->bytes_limit = BN_AUDIO_DEMUX_BYTES_LIMIT;
        return;
    }
    demux->horizon_us = BN_READ_AHEAD_LOCAL_US;
    demux->bytes_floor = BN_AUDIO_READ_AHEAD_FLOOR_BYTES;
    demux->bytes_limit =
        bn_demux_span_bytes(demux, 2 * BN_READ_AHEAD_LOCAL_US, BN_AUDIO_READ_AHEAD_FLOOR_BYTES,
                            BN_AUDIO_DEMUX_BYTES_LIMIT, BN_AUDIO_DEMUX_BYTES_LIMIT);
}

/* Describes the source after the demux may already be bound: re-sized under the lock, and the
 * reader woken, since a queue that was full may not be any more (and the other way round). */
void bn_demux_set_source_local(BnDemux *demux, int local) {
    if (!demux->thread) {
        demux->source_local = local != 0;
        if (demux->format)
            bn_demux_size(demux);
        return;
    }
    AcquireSRWLockExclusive(&demux->lock);
    demux->source_local = local != 0;
    bn_demux_size(demux);
    WakeAllConditionVariable(&demux->changed);
    ReleaseSRWLockExclusive(&demux->lock);
}

static int64_t bn_packet_pts(const AVPacket *packet) {
    return packet->pts != AV_NOPTS_VALUE ? packet->pts : packet->dts;
}

/* Whether `target` is inside what the queue holds, so a seek to it can be served from memory. */
int bn_demux_covers_locked(const BnDemux *demux, int64_t target) {
    return demux->count > 0 && !demux->seek_pending && !demux->error &&
           demux->newest_pts != AV_NOPTS_VALUE && target <= demux->newest_pts;
}

/* Drops the queue's first `count` packets. */
static void bn_demux_drop_front_locked(BnDemux *demux, int count) {
    while (count-- > 0 && demux->count > 0) {
        AVPacket *packet = demux->ring[demux->head];
        demux->ring[demux->head] = NULL;
        demux->head = (demux->head + 1) % demux->capacity;
        demux->count -= 1;
        demux->bytes -= (size_t) packet->size;
        bn_demux_census(demux, packet, -1);
        av_packet_free(&packet);
    }
}

/* Serves a backward seek from the queue: the queued packet with the greatest pts at or before
 * `target` becomes the queue's first, and decoding from it
 * reaches the target without the container moving. Returns 1 when it did, 0 when nothing queued
 * qualifies. Packets sit in decode order, so pts is not monotonic across a reordered run and the
 * whole queue is examined; a seek is rare enough for that. */
int bn_demux_trim_to_locked(BnDemux *demux, int64_t target) {
    if (!bn_demux_covers_locked(demux, target))
        return 0;
    int best = -1;
    int64_t best_pts = AV_NOPTS_VALUE;
    for (int index = 0; index < demux->count; ++index) {
        const AVPacket *packet = demux->ring[(demux->head + index) % demux->capacity];
        const int64_t pts = bn_packet_pts(packet);
        if (pts == AV_NOPTS_VALUE || pts > target)
            continue;
        if (best < 0 || pts > best_pts) {
            best = index;
            best_pts = pts;
        }
    }
    if (best < 0)
        return 0;
    bn_demux_drop_front_locked(demux, best);
    return 1;
}

/* How many times in a row a transport error is answered with a reposition before it is reported. */
#define BN_DEMUX_READ_RETRIES 2

/* Whether a failed read or seek is worth one more try on a fresh connection.
 *
 * The http layer keeps one connection open across Range requests (`multiple_requests`), and a CDN
 * closes an idle one without telling anybody: the next request goes out on a dead socket, reads
 * EOF where the response should be, and FFmpeg reports an I/O error instead of reconnecting. That
 * is not the network failing — the very next connection works — but reported upwards it ended the
 * session, and the viewer sat through a reopen for what was one stale socket (a live seek
 * probe). A TIMEOUT is different: the link is silent, the http layer has already spent
 * its own reconnect budget on it, and the owner's deadline is what bounds that wait. */
static int bn_demux_error_is_recoverable(int error) {
    /* Cancellation and operation deadlines are terminal, not fresh reconnect budgets. */
    return error != AVERROR_EOF && error != AVERROR_EXIT &&
           error != AVERROR(ETIMEDOUT) && error != AVERROR(ENOMEM);
}

/* What a failed read really was. The byte layer reports a transport failure ONCE and answers
 * end-of-file from then on, so a read that lost its connection arrives here looking exactly like
 * the end of the container — and treated as one, a seek "reached the end of the stream before its
 * target" a hundred seconds into a five-minute file. The byte context keeps the real error. */
static int bn_demux_read_error(const BnDemux *demux, int result, int64_t reached) {
    AVIOContext *io = demux->format->pb;
    if (result != AVERROR_EOF || !io)
        return result;
    if (io->error < 0 && io->error != AVERROR_EOF)
        return io->error;
    /* A request that was hung up on carries no error at all — not even the byte layer's own
     * end-of-file flag survives the demuxer's handling of it. The arithmetic does not lie, though:
     * an end that falls short of the resource in BYTES and short of the stream in TIME did not
     * end, it was cut off. Both are asked for because each has an innocent reading alone: a
     * container may close with boxes nobody reads, and a stream may be shorter than its file. */
    const int64_t size = avio_size(io);
    const int64_t position = avio_tell(io);
    if (size <= 0 || position < 0 || position >= size)
        return result;
    const AVStream *stream = demux->format->streams[demux->stream_index];
    int64_t duration = stream->duration;
    if (duration == AV_NOPTS_VALUE && demux->format->duration > 0)
        duration = av_rescale_q(demux->format->duration, AV_TIME_BASE_Q, stream->time_base);
    if (duration == AV_NOPTS_VALUE || reached == AV_NOPTS_VALUE)
        /* No clock to ask: only a gap too large to be a container's closing boxes counts. */
        return size - position > (1 << 20) ? AVERROR(EIO) : result;
    const int64_t start = stream->start_time != AV_NOPTS_VALUE ? stream->start_time : 0;
    const int64_t slack = av_rescale_q(2, (AVRational){1, 1}, stream->time_base);
    return reached + slack < start + duration ? AVERROR(EIO) : result;
}

/* Where a DSF's sound starts: the demuxer accepts only a 28-byte `DSD ` chunk and a 52-byte `fmt `
 * chunk ahead of the 12-byte `data` header. */
#define BN_DSF_DATA_OFFSET 92

/* Repositions the container, as `avformat_seek_file`.
 *
 * FFmpeg's DSF demuxer has no seek of its own (`AVFMT_GENERIC_INDEX | AVFMT_NO_BYTE_SEEK`): its
 * generic seek reads every packet from the last one it indexed up to the target. A seek to the
 * middle of a song read half the file first — on a network share, cold, longer than the read
 * deadline (the DSF reader test's "DSD packet timeout", hidden whenever the file was cached). Its
 * blocks are a fixed size and a packet's time is where it starts, so the block holding the target
 * is found by arithmetic and the byte layer moved there. */
static int bn_demux_seek_file(BnDemux *demux, int64_t min_ts, int64_t target, int64_t max_ts,
                              int flags) {
    AVFormatContext *format = demux->format;
    const AVCodecParameters *codec = format->streams[demux->stream_index]->codecpar;
    const int channels = codec->ch_layout.nb_channels;
    if (strcmp(format->iformat->name, "dsf") != 0 || !format->pb || channels <= 0 ||
        codec->block_align <= 0 || codec->block_align % channels != 0 || target < 0 ||
        (flags & AVSEEK_FLAG_BYTE))
        return avformat_seek_file(format, demux->stream_index, min_ts, target, max_ts, flags);
    /* A packet's time counts byte-frames: a block of `block_align` bytes holds that many over the
     * channel count. */
    const int64_t block_frames = codec->block_align / channels;
    int64_t block = target / block_frames;
    if (block * block_frames < min_ts)
        block += 1;
    if (block * block_frames > max_ts)
        return AVERROR(EINVAL);
    avformat_flush(format);
    const int64_t position = avio_seek(format->pb, BN_DSF_DATA_OFFSET + block * codec->block_align,
                                       SEEK_SET);
    return position < 0 ? (int) position : 0;
}

/* Repositions the container where the reader had got to, which makes the http layer open a new
 * connection. Called WITHOUT the lock; the caller re-checks the epoch. Returns the seek's result. */
static int bn_demux_recover(BnDemux *demux, int64_t resume_ts) {
    const AVStream *stream = demux->format->streams[demux->stream_index];
    /* The byte context latches its error and its end-of-file; a seek clears only the latter. */
    if (demux->format->pb) {
        demux->format->pb->error = 0;
        demux->format->pb->eof_reached = 0;
    }
    int64_t target = resume_ts;
    if (target == AV_NOPTS_VALUE)
        target = stream->start_time != AV_NOPTS_VALUE ? stream->start_time : 0;
#ifdef BN_INSTRUMENT
    fprintf(stderr, "player-core[demux] transport error: reconnecting at %lld\n", (long long) target);
#endif
    bn_interrupt_deadline(demux->interrupt, BN_READ_DEADLINE_US);
    const int result = bn_demux_seek_file(demux, INT64_MIN, target, target, AVSEEK_FLAG_BACKWARD);
    bn_interrupt_deadline(demux->interrupt, 0);
    return result;
}

static DWORD WINAPI bn_demux_main(LPVOID opaque) {
    BnDemux *demux = (BnDemux *) opaque;
    AVPacket *packet = av_packet_alloc();
    AcquireSRWLockExclusive(&demux->lock);
    if (!packet) {
        demux->error = AVERROR(ENOMEM);
        WakeAllConditionVariable(&demux->changed);
    }
    while (packet && !demux->stop) {
        if (demux->seek_pending) {
            const uint64_t epoch = demux->epoch;
            const int64_t min_ts = demux->seek_min, target = demux->seek_target,
                          max_ts = demux->seek_max;
            const int flags = demux->seek_flags;
            demux->seek_pending = 0;
            ReleaseSRWLockExclusive(&demux->lock);
            /* A latched byte-layer error belongs to the position being left. */
            if (demux->format->pb)
                demux->format->pb->error = 0;
            bn_interrupt_deadline(demux->interrupt, BN_READ_DEADLINE_US);
            const int result = bn_demux_seek_file(demux, min_ts, target, max_ts, flags);
            bn_interrupt_deadline(demux->interrupt, 0);
#ifdef BN_INSTRUMENT
            if (result < 0)
                fprintf(stderr, "player-core[demux] seek to %lld failed: %s\n", (long long) target,
                        av_err2str(result));
#endif
            AcquireSRWLockExclusive(&demux->lock);
            if (demux->epoch == epoch && result < 0 && !demux->stop &&
                bn_demux_error_is_recoverable(result) && demux->retries < BN_DEMUX_READ_RETRIES) {
                /* The same seek again: the failed one closed its connection on the way out. */
                demux->retries += 1;
                demux->seek_pending = 1;
                continue;
            }
            if (demux->epoch == epoch && result < 0) {
                /* Past the end of a container that cannot seek there, there is nothing to read:
                 * the decoder drains to its last frame, as it does at a real end. */
                if (result == AVERROR_EOF)
                    demux->eof = 1;
                else
                    demux->error = result;
                WakeAllConditionVariable(&demux->changed);
            }
            continue;
        }
        if (demux->eof || demux->error || bn_demux_full_locked(demux)) {
            SleepConditionVariableSRW(&demux->changed, &demux->lock, INFINITE, 0);
            continue;
        }
        const uint64_t epoch = demux->epoch;
        /* EOF normalization does I/O outside the lock, but seek state belongs to this epoch. */
        const int64_t reached = demux->last_dts != AV_NOPTS_VALUE ? demux->last_dts
                                : demux->has_seek_target ? demux->seek_target : AV_NOPTS_VALUE;
        ReleaseSRWLockExclusive(&demux->lock);
        bn_interrupt_deadline(demux->interrupt, BN_READ_DEADLINE_US);
        const int read_result = av_read_frame(demux->format, packet);
        bn_interrupt_deadline(demux->interrupt, 0);
        int result = read_result < 0 ? bn_demux_read_error(demux, read_result, reached) : read_result;
#ifdef BN_INSTRUMENT
        if (result < 0 && result != AVERROR_EOF)
            fprintf(stderr, "player-core[demux] read failed: %s\n", av_err2str(result));
#endif
        const int truncated = result >= 0 && (packet->flags & AV_PKT_FLAG_CORRUPT) &&
                              packet->stream_index == demux->stream_index;
#ifdef BN_INSTRUMENT
        if (truncated)
            fprintf(stderr, "player-core[demux] corrupt packet stream=%d size=%d pts=%lld pos=%lld\n",
                    packet->stream_index, packet->size, (long long) packet->pts,
                    (long long) packet->pos);
#endif
        AcquireSRWLockExclusive(&demux->lock);
        if (demux->stop || demux->epoch != epoch || demux->seek_pending) {
            /* A seek arrived while the read was in flight: whatever it read is from before it. */
            av_packet_unref(packet);
            continue;
        }
        if (truncated && demux->retries < BN_DEMUX_READ_RETRIES) {
            /* The container marks a packet it could only read PART of: the bytes stopped coming
             * mid-sample. That is the transport failing, not the media being bad — and queued as
             * it was, the decoder rejected it seconds later and the whole session was reopened
             * for it (twice in one evening's log, both times a session's very first packet). It
             * is read again on a new connection instead. Once the retries are spent the packet
             * goes through as it is: a file that really is damaged there is the decoder's call. */
            av_packet_unref(packet);
            result = AVERROR(EIO);
        }
        if (result < 0 && bn_demux_error_is_recoverable(result) &&
            demux->retries < BN_DEMUX_READ_RETRIES) {
            /* Carry on from the last packet read, on a new connection. What the reposition reads
             * again up to that packet is already queued (or already played) and is dropped. */
            demux->retries += 1;
            const int64_t resumed_from = demux->last_dts;
            const int64_t resume_ts = resumed_from != AV_NOPTS_VALUE ? resumed_from
                                      : demux->has_seek_target      ? demux->seek_target
                                                                    : AV_NOPTS_VALUE;
            ReleaseSRWLockExclusive(&demux->lock);
            const int recovered = bn_demux_recover(demux, resume_ts);
            AcquireSRWLockExclusive(&demux->lock);
            if (demux->stop || demux->epoch != epoch || demux->seek_pending)
                continue;
            if (recovered >= 0) {
                demux->skip_through_dts = resumed_from;
                continue;
            }
#ifdef BN_INSTRUMENT
            fprintf(stderr, "player-core[demux] reconnect failed: %s\n", av_err2str(recovered));
#endif
        }
        if (result < 0) {
            /* Preserve the normalized transport error after recovery is exhausted. The original
             * demux return can be EOF even though the byte layer failed or was truncated. */
            if (result == AVERROR_EOF)
                demux->eof = 1;
            else
                demux->error = result;
            WakeAllConditionVariable(&demux->changed);
            continue;
        }
        if (packet->stream_index != demux->stream_index) {
            av_packet_unref(packet);
            continue;
        }
        {
            const int64_t dts = packet->dts != AV_NOPTS_VALUE ? packet->dts : packet->pts;
            if (demux->skip_through_dts != AV_NOPTS_VALUE) {
                if (dts != AV_NOPTS_VALUE && dts <= demux->skip_through_dts) {
                    av_packet_unref(packet);
                    continue;
                }
                demux->skip_through_dts = AV_NOPTS_VALUE;
            }
            if (dts != AV_NOPTS_VALUE)
                demux->last_dts = dts;
            demux->retries = 0;
        }
        AVPacket *queued = av_packet_alloc();
        if (!queued) {
            demux->error = AVERROR(ENOMEM);
            av_packet_unref(packet);
            WakeAllConditionVariable(&demux->changed);
            continue;
        }
        av_packet_move_ref(queued, packet);
        const int tail = (demux->head + demux->count) % demux->capacity;
        demux->ring[tail] = queued;
        demux->count += 1;
        demux->bytes += (size_t) queued->size;
        bn_demux_census(demux, queued, 1);
        const int64_t pts = queued->pts != AV_NOPTS_VALUE ? queued->pts : queued->dts;
        if (pts != AV_NOPTS_VALUE &&
            (demux->newest_pts == AV_NOPTS_VALUE || pts > demux->newest_pts))
            demux->newest_pts = pts;
        WakeAllConditionVariable(&demux->changed);
    }
    ReleaseSRWLockExclusive(&demux->lock);
    av_packet_free(&packet);
    return 0;
}

/* Starts the demux thread on first use: the container is open and nothing else reads it any more. */
int bn_demux_ensure(BnDemux *demux) {
    if (demux->thread)
        return 0;
    if (!demux->format || demux->stream_index < 0 || !demux->interrupt)
        return AVERROR(EINVAL);
    demux->capacity = BN_DEMUX_QUEUE_PACKETS_LIMIT;
    demux->ring = (AVPacket **) calloc((size_t) demux->capacity, sizeof(*demux->ring));
    if (!demux->ring)
        return AVERROR(ENOMEM);
    InitializeSRWLock(&demux->lock);
    InitializeConditionVariable(&demux->changed);
    demux->newest_pts = AV_NOPTS_VALUE;
    demux->last_dts = AV_NOPTS_VALUE;
    demux->skip_through_dts = AV_NOPTS_VALUE;
    demux->thread = CreateThread(NULL, 0, bn_demux_main, demux, 0, NULL);
    if (!demux->thread) {
        free(demux->ring);
        demux->ring = NULL;
        return AVERROR(ENOMEM);
    }
    return 0;
}

/* Takes the next packet into `out`. Returns 1 with a packet, 0 when none is queued yet (only
 * after waiting `BN_AUDIO_TAKE_WAIT_MS`), AVERROR_EOF at the end, or the container's error. */
int bn_demux_take(BnDemux *demux, AVPacket *out) {
    const int started = bn_demux_ensure(demux);
    if (started < 0)
        return started;
    int result = 0;
    AcquireSRWLockExclusive(&demux->lock);
    for (;;) {
        if (demux->count > 0 && !demux->seek_pending) {
            AVPacket *packet = demux->ring[demux->head];
            demux->ring[demux->head] = NULL;
            demux->head = (demux->head + 1) % demux->capacity;
            demux->count -= 1;
            demux->bytes -= (size_t) packet->size;
            bn_demux_census(demux, packet, -1);
            av_packet_move_ref(out, packet);
            av_packet_free(&packet);
            WakeAllConditionVariable(&demux->changed);
            result = 1;
            break;
        }
        if (!demux->seek_pending && demux->error) {
            result = demux->error;
            break;
        }
        if (!demux->seek_pending && demux->eof) {
            result = AVERROR_EOF;
            break;
        }
        if (!SleepConditionVariableSRW(&demux->changed, &demux->lock, BN_AUDIO_TAKE_WAIT_MS, 0)) {
            result = 0;
            break;
        }
    }
    ReleaseSRWLockExclusive(&demux->lock);
    return result;
}

/* Repositions the container on the demux thread. Returns at once: the queue is emptied here, and
 * the next packet a take returns is one read after the reposition. */
int bn_demux_request_seek(BnDemux *demux, int64_t min_ts, int64_t target,
                          int64_t max_ts, int flags) {
    const int started = bn_demux_ensure(demux);
    if (started < 0)
        return started;
    AcquireSRWLockExclusive(&demux->lock);
    demux->epoch += 1;
    bn_demux_clear_locked(demux);
    demux->seek_pending = 1;
    demux->seek_min = min_ts;
    demux->seek_target = target;
    demux->seek_max = max_ts;
    demux->seek_flags = flags;
    demux->has_seek_target = 1;
    demux->last_dts = AV_NOPTS_VALUE;
    demux->skip_through_dts = AV_NOPTS_VALUE;
    demux->retries = 0;
    demux->eof = 0;
    demux->error = 0;
    WakeAllConditionVariable(&demux->changed);
    ReleaseSRWLockExclusive(&demux->lock);
    return 0;
}

/* Stops the demux thread and frees its queue. A read blocked on the network is interrupted. */
void bn_demux_stop(BnDemux *demux) {
    if (!demux->thread)
        return;
    AcquireSRWLockExclusive(&demux->lock);
    demux->stop = 1;
    /* Cancellation is separate from deadlines; an operation cannot reset it accidentally. */
    InterlockedExchange(&demux->interrupt->cancelled, 1);
    WakeAllConditionVariable(&demux->changed);
    ReleaseSRWLockExclusive(&demux->lock);
    WaitForSingleObject(demux->thread, INFINITE);
    CloseHandle(demux->thread);
    demux->thread = NULL;
    bn_demux_clear_locked(demux);
    free(demux->ring);
    demux->ring = NULL;
    bn_interrupt_deadline(demux->interrupt, 0);
}

/* Tells the demux thread to stop, without waiting for it: a read blocked on the network is
 * interrupted and the thread leaves on its own. `bn_audio_decoder_destroy` — which does wait — is
 * then cheap, and belongs on a thread that may wait. */
void bn_demux_cancel(BnDemux *demux) {
    if (!demux->thread)
        return;
    AcquireSRWLockExclusive(&demux->lock);
    demux->stop = 1;
    InterlockedExchange(&demux->interrupt->cancelled, 1);
    WakeAllConditionVariable(&demux->changed);
    ReleaseSRWLockExclusive(&demux->lock);
}
