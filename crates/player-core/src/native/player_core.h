#ifndef PLAYER_CORE_H
#define PLAYER_CORE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct BnAudioDecoder BnAudioDecoder;
typedef struct BnUrlSource BnUrlSource;

typedef struct BnByteSource {
    void *opaque;
    int32_t (*read)(void *opaque, uint8_t *buffer, int32_t buffer_size);
    int64_t (*seek)(void *opaque, int64_t offset, int32_t whence);
    /* Nonblocking cancellation query; optional. Same lifetime as opaque. */
    int (*cancelled)(void *opaque);
    /* Optional read bounded by the owning decoder operation's remaining microseconds. */
    int32_t (*read_timed)(void *opaque, uint8_t *buffer, int32_t size, int64_t budget_us);
    /* Optional UTF-16 file identity, valid for the lifetime of the source. */
    const uint16_t *local_path;
} BnByteSource;

/* Options handed to FFmpeg's protocol layer on open, as an AVDictionary. NULL, or a struct whose
 * fields are NULL/empty, sends no custom headers; finite transport budgets still apply.
 * The CDN that needs a Referer and the
 * local file that needs nothing take ONE open path, and the dict is the whole difference.
 * `headers` is the CRLF-terminated "Name: value" block avformat's http protocol expects. Neither
 * field ever carries a cookie: this struct exists for a CDN's origin check, not for credentials. */
typedef struct BnHttpOpenOptions {
    const char *headers;
    const char *user_agent;
} BnHttpOpenOptions;

typedef struct BnFrameTiming {
    int64_t pts;
    int32_t has_timestamp;
    int32_t used_best_effort_timestamp;
} BnFrameTiming;

typedef struct BnAudioStreamInfo {
    int32_t time_base_num;
    int32_t time_base_den;
    int64_t start_time;
    int32_t has_start_time;
    int64_t format_start_time_us;
    int32_t has_format_start_time;
    int32_t source_sample_rate;
    int32_t source_channels;
    int32_t output_sample_rate;
    int32_t output_channels;
    int32_t stream_index;
    /* AVChannelLayout's native speaker bits when the source declares them. Zero means the
       source did not declare positions; only mono/stereo are filled by the explicit fallback. */
    uint64_t source_layout_mask;
    int32_t source_layout_assumed;
} BnAudioStreamInfo;

#define BN_AUDIO_TRACK_LANGUAGE_CAPACITY 64
#define BN_AUDIO_TRACK_CODEC_CAPACITY 64

typedef struct BnAudioTrackInfo {
    int32_t stream_index;
    int32_t channels;
    char language[BN_AUDIO_TRACK_LANGUAGE_CAPACITY];
    char codec[BN_AUDIO_TRACK_CODEC_CAPACITY];
} BnAudioTrackInfo;

typedef struct BnAudioChunk {
    const float *samples;
    int32_t frames;
    BnFrameTiming timing;
} BnAudioChunk;

/* Raw DSD bytes. `frames` counts bytes per channel: every byte contains eight MSB-first DSD
 * samples, and `bytes` is interleaved by channel (`frame * channels + channel`). */
typedef struct BnDsdChunk {
    const uint8_t *bytes;
    int32_t frames;
    BnFrameTiming timing;
} BnDsdChunk;

/* Status of an open (a decoder, a URL source). The audio decoder's own opens return these. */
enum BnOpenStatus {
    BN_OPEN_OK = 0,
    BN_OPEN_INVALID_ARGUMENT = -1,
    BN_OPEN_INPUT = -2,
    BN_OPEN_STREAM_INFO = -3,
    BN_OPEN_NO_STREAM = -4,
    BN_OPEN_INVALID_TIME_BASE = -5,
    BN_OPEN_DECODER_ALLOCATION = -6,
    BN_OPEN_CODEC_PARAMETERS = -7,
    BN_OPEN_CODEC = -9,
    BN_OPEN_PACKET_ALLOCATION = -10,
    BN_OPEN_FRAME_ALLOCATION = -11,
};

enum BnAudioStatus {
    BN_AUDIO_OK = 0,
    BN_AUDIO_EOF = 1,
    /* No packet arrived within the wait slice: the network is still delivering. Not an error. */
    BN_AUDIO_AGAIN = 2,
    BN_AUDIO_INVALID_ARGUMENT = -101,
    BN_AUDIO_INPUT = -102,
    BN_AUDIO_STREAM_INFO = -103,
    BN_AUDIO_NO_STREAM = -104,
    BN_AUDIO_INVALID_TIME_BASE = -105,
    BN_AUDIO_DECODER_ALLOCATION = -106,
    BN_AUDIO_CODEC_PARAMETERS = -107,
    BN_AUDIO_CODEC = -108,
    BN_AUDIO_PACKET_ALLOCATION = -109,
    BN_AUDIO_FRAME_ALLOCATION = -110,
    BN_AUDIO_RESAMPLER = -111,
    BN_AUDIO_BUFFER_ALLOCATION = -112,
    BN_AUDIO_READ = -113,
    BN_AUDIO_SEND = -114,
    BN_AUDIO_RECEIVE = -115,
    BN_AUDIO_CONVERT = -116,
    BN_AUDIO_SEEK = -117,
    BN_AUDIO_INVALID_STREAM = -118,
    BN_AUDIO_BUFFER_TOO_SMALL = -119,
};

/* A network resource turned into a POSITIONAL byte reader, through the same protocol layer and
 * the same `BnHttpOpenOptions` a decoder open takes. `bn_url_source_read` returns the byte count,
 * 0 at end of resource, and a negative BnOpenStatus otherwise; `bn_url_source_size` returns the
 * declared size, or negative when the transport declared none. */
/* Callback/opaque are borrowed through destroy (including failed-open cleanup). The caller
 * owns their storage independently of the source; cancellation never dereferences a source. */
int32_t bn_url_source_open_with_error(const char *url, const BnHttpOpenOptions *http,
                           int (*cancelled)(void *), void *cancel_opaque,
                           BnUrlSource **out_source, char *error,
                           size_t error_capacity, int32_t *native_error);
/* AVERROR/errno classification lives beside the SDK constants, never in message parsing. */
int32_t bn_error_is_network_failure(int32_t code);
int32_t bn_io_error_code(int32_t kind);
void bn_url_source_destroy(BnUrlSource **source);
int64_t bn_url_source_size(const BnUrlSource *source);
int32_t bn_url_source_read(BnUrlSource *source, int64_t offset,
                           uint8_t *buffer, int32_t size, int64_t budget_us);

/* Measurement only: what every demux queue in the process holds right now - packet payload,
 * packets, and the bytes of the buffers carrying them. */
typedef struct BnBufferCensus {
    int64_t audio_queue_bytes;
    int64_t audio_queue_packets;
    int64_t audio_queue_buffer_bytes;
} BnBufferCensus;
void bn_buffer_census(BnBufferCensus *out);

int32_t bn_audio_decoder_open(const char *local_path,
                              int32_t output_sample_rate,
                              int32_t output_channels,
                              BnAudioDecoder **out_decoder,
                              BnAudioStreamInfo *out_info, char *error,
                              size_t error_capacity);
int32_t bn_audio_decoder_open_source(const BnByteSource *source,
                                     int32_t output_sample_rate,
                                     int32_t output_channels,
                                     BnAudioDecoder **out_decoder,
                                     BnAudioStreamInfo *out_info, char *error,
                                     size_t error_capacity);
int32_t bn_audio_decoder_open_source_stream_with_error(
    const BnByteSource *source, int32_t audio_stream_index,
    int32_t output_sample_rate, int32_t output_channels,
    BnAudioDecoder **out_decoder, BnAudioStreamInfo *out_info, char *error,
    size_t error_capacity, int32_t *native_error);
int32_t bn_audio_decoder_transport_error(const BnAudioDecoder *decoder);
int32_t bn_audio_tracks_enumerate(const char *local_path,
                                  BnAudioTrackInfo *out_tracks,
                                  size_t track_capacity,
                                  size_t *out_track_count, char *error,
                                  size_t error_capacity);

/* What a container says about itself. Metadata text can be UTF-8 or a legacy code page and
 * may be cut at the buffer's end, so a reader drops an unfinished last sequence. `has_video` is a picture that
 * MOVES: a cover (AV_DISPOSITION_ATTACHED_PIC) is not one, and comes back in `picture` as the
 * encoded image the container holds (JPEG or PNG), owned by this struct until
 * `bn_media_tags_free`. `lyrics` is the lyrics tag whole — `lyrics_size` metadata bytes, NOT
 * nul-terminated, owned the same way: a song's words do not fit a fixed buffer. */
typedef struct BnMediaTags {
    /* Every tag the file holds, as `key NUL value NUL` pairs: the container's, then the first
     * audio stream's. What each means is the Rust side's rule (`tag_rule.rs`). */
    char *metadata;
    size_t metadata_size;
    int64_t duration_us;
    int32_t has_duration;
    int32_t has_audio;
    int32_t has_video;
    uint8_t *picture;
    size_t picture_size;
    /* The first audio stream: codec name, rate in Hz, the depth the decoder states and the one
     * the container codes samples in (0 when not said), and the rate in bits per second (the
     * stream's, else the container's; 0 when neither says). */
    char audio_codec[32];
    int32_t audio_sample_rate;
    int32_t audio_bits_raw;
    int32_t audio_bits_coded;
    int64_t audio_bit_rate;
    /* The container's format by FFmpeg's short name — the first of a demuxer's list, so MP4 and
     * M4A are "mov". */
    char container[32];
} BnMediaTags;

int32_t bn_media_tags_probe_source(const BnByteSource *source,
                                   BnMediaTags *out_tags, char *error,
                                   size_t error_capacity);
void bn_media_tags_free(BnMediaTags *tags);
void bn_audio_decoder_cancel(BnAudioDecoder *decoder);
/* Whether the sound's input is a local file (1) or arrives over a network (0, what a decoder
 * nobody described is taken for): it sizes how far ahead the input is read. */
void bn_audio_decoder_set_source_local(BnAudioDecoder *decoder, int32_t local);
int32_t bn_audio_decoder_input_state(BnAudioDecoder *decoder, int64_t *out_queued_us,
                                     int32_t *out_ended);
/* Source frames handed to the resampler since the decoder opened. */
int64_t bn_audio_decoder_resampler_input_frames(const BnAudioDecoder *decoder);
/* Codec frames the decoder could not decode and passed over since it opened. Each plays as the
 * silence of its span: `bn_audio_decoder_decode_next` hands that silence out, where the frame
 * stood, before the next frame that decoded. */
int64_t bn_audio_decoder_passed_over(const BnAudioDecoder *decoder);
void bn_audio_decoder_destroy(BnAudioDecoder **decoder);
const char *bn_audio_decoder_codec_name(const BnAudioDecoder *decoder);
const char *bn_audio_decoder_tag(const BnAudioDecoder *decoder, const char *name);
int32_t bn_audio_decoder_seek(BnAudioDecoder *decoder, int64_t target_pts,
                              char *error, size_t error_capacity);
int32_t bn_audio_decoder_enable_dsd(BnAudioDecoder *decoder,
                                    BnAudioStreamInfo *out_info, char *error,
                                    size_t error_capacity);
int32_t bn_audio_decoder_read_dsd(BnAudioDecoder *decoder, BnDsdChunk *out_chunk,
                                  char *error, size_t error_capacity);
int32_t bn_audio_decoder_decode_next(BnAudioDecoder *decoder,
                                     BnAudioChunk *out_chunk, char *error,
                                     size_t error_capacity);

#ifdef __cplusplus
}
#endif

#endif
