/* What a container says about itself: its tags, its length, whether it carries a picture that
 * moves, and the cover it has attached. A probe, not a session: it opens the container, reads,
 * and closes it, and nothing it opens outlives the call. */
#include "player_core_internal.h"

/* Appends every entry of `metadata` to the blob as `key NUL value NUL`. Returns 0, or
 * AVERROR(ENOMEM) when the blob cannot grow (what it holds so far stays valid). */
static int bn_metadata_append(BnMediaTags *tags, const AVDictionary *metadata) {
    const AVDictionaryEntry *entry = NULL;
    while ((entry = av_dict_get(metadata, "", entry, AV_DICT_IGNORE_SUFFIX))) {
        const size_t key = strlen(entry->key) + 1;
        const size_t value = strlen(entry->value) + 1;
        char *grown = av_realloc(tags->metadata, tags->metadata_size + key + value);
        if (!grown)
            return AVERROR(ENOMEM);
        memcpy(grown + tags->metadata_size, entry->key, key);
        memcpy(grown + tags->metadata_size + key, entry->value, value);
        tags->metadata = grown;
        tags->metadata_size += key + value;
    }
    return 0;
}

void bn_media_tags_free(BnMediaTags *tags) {
    if (!tags)
        return;
    av_freep(&tags->picture);
    tags->picture_size = 0;
    av_freep(&tags->metadata);
    tags->metadata_size = 0;
}

int32_t bn_media_tags_probe_source(const BnByteSource *source,
                                   BnMediaTags *out_tags, char *error,
                                   size_t error_capacity) {
    if (!source || !source->opaque || !source->read || !source->seek ||
        !out_tags) {
        bn_error(error, error_capacity,
                 "media tag probe received an invalid argument");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    memset(out_tags, 0, sizeof(*out_tags));

    AVFormatContext *format = NULL;
    AVIOContext *custom_io = NULL;
    BnInterrupt interrupt = {0};
    int result = bn_custom_input_open(&format, &custom_io, source, &interrupt);
    if (result < 0) {
        bn_av_error(error, error_capacity,
                    "could not open input for the media tag probe", result);
        avformat_close_input(&format);
        bn_custom_io_free(&custom_io);
        return BN_AUDIO_INPUT;
    }
    /* The tags and the attached picture are complete at the header. The stream scan is for the
     * length: an MP3 without a Xing header has none until its frames are counted. */
    result = avformat_find_stream_info(format, NULL);
    if (bn_interrupt_expired(&interrupt))
        result = AVERROR_EXIT;
    if (result < 0) {
        bn_av_error(error, error_capacity,
                    "could not read stream information for the media tag probe",
                    result);
        avformat_close_input(&format);
        bn_custom_io_free(&custom_io);
        return BN_AUDIO_STREAM_INFO;
    }

    const AVStream *audio = NULL;
    const AVStream *cover = NULL;
    for (unsigned int index = 0; index < format->nb_streams; ++index) {
        const AVStream *stream = format->streams[index];
        switch (stream->codecpar->codec_type) {
        case AVMEDIA_TYPE_AUDIO:
            if (!audio)
                audio = stream;
            break;
        case AVMEDIA_TYPE_VIDEO:
            /* A cover is a video stream to FFmpeg — one frame, carried whole in the header. It
             * is not a picture that moves, and a file that has only that is a sound file. */
            if (stream->disposition & AV_DISPOSITION_ATTACHED_PIC) {
                if (!cover && stream->attached_pic.size > 0)
                    cover = stream;
            } else {
                out_tags->has_video = 1;
            }
            break;
        default:
            break;
        }
    }
    out_tags->has_audio = audio ? 1 : 0;

    /* The tags as the file holds them; a blob that could not grow keeps what it has. */
    if (bn_metadata_append(out_tags, format->metadata) == 0 && audio)
        bn_metadata_append(out_tags, audio->metadata);
    /* The demuxer's short name, up to the first comma of its list. */
    if (format->iformat && format->iformat->name) {
        bn_copy_text(out_tags->container, sizeof(out_tags->container),
                     format->iformat->name);
        char *comma = strchr(out_tags->container, ',');
        if (comma)
            *comma = '\0';
    }

    if (format->duration != AV_NOPTS_VALUE && format->duration > 0) {
        out_tags->has_duration = 1;
        out_tags->duration_us = format->duration; /* AV_TIME_BASE is microseconds. */
    }

    /* The sound's own resolution, as the container states it: what a listener reads as
     * "96 kHz / 24-bit ALAC". Whether the codec is lossless, and so has a depth, is the Rust
     * side's rule. */
    if (audio) {
        const AVCodecParameters *parameters = audio->codecpar;
        bn_copy_text(out_tags->audio_codec, sizeof(out_tags->audio_codec),
                     avcodec_get_name(parameters->codec_id));
        out_tags->audio_sample_rate = parameters->sample_rate;
        out_tags->audio_bits_raw = parameters->bits_per_raw_sample;
        out_tags->audio_bits_coded = parameters->bits_per_coded_sample;
        const int64_t rate =
            parameters->bit_rate > 0 ? parameters->bit_rate : format->bit_rate;
        out_tags->audio_bit_rate = rate > 0 ? rate : 0;
    }

    int32_t status = BN_AUDIO_OK;
    if (cover) {
        const size_t size = (size_t) cover->attached_pic.size;
        out_tags->picture = av_malloc(size);
        if (out_tags->picture) {
            memcpy(out_tags->picture, cover->attached_pic.data, size);
            out_tags->picture_size = size;
        } else {
            /* A file with no cover is still a file with tags. */
            out_tags->picture_size = 0;
        }
    }

    avformat_close_input(&format);
    bn_custom_io_free(&custom_io);
    return status;
}
