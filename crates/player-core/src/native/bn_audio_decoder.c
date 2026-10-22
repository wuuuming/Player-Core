/* The audio decoder handle: track enumeration, opening over a path or a byte source, seeking,
 * and decoding into resampled output samples. */
#include "player_core_internal.h"

/* The audio decoder's demux, bound to its container on first use. */
static BnDemux *bn_audio_demux(BnAudioDecoder *decoder) {
    BnDemux *demux = &decoder->demux;
    if (!demux->format) {
        demux->format = decoder->format;
        demux->stream_index = decoder->audio_index;
        demux->interrupt = &decoder->interrupt;
        bn_demux_size(demux);
        /* A read blocked inside FFmpeg's own protocols polls this; a custom byte source is
         * cancelled by its owner. */
        decoder->format->interrupt_callback.callback = bn_interrupt_expired;
        decoder->format->interrupt_callback.opaque = &decoder->interrupt;
    }
    return demux;
}

/* The mixer needs positions, not just a channel count. FFmpeg explicitly represents an unknown
 * layout as UNSPEC; do not turn an unknown multichannel stream into a guessed surround layout.
 * Mono and stereo are the two deliberately documented fallbacks, carried with `assumed=1`. */
static void bn_audio_source_layout_info(const AVChannelLayout *layout,
                                        BnAudioStreamInfo *info) {
    info->source_layout_mask = 0;
    info->source_layout_assumed = layout->order != AV_CHANNEL_ORDER_NATIVE;
    if (layout->order == AV_CHANNEL_ORDER_NATIVE) {
        info->source_layout_mask = layout->u.mask;
        return;
    }
    if (layout->nb_channels == 1 || layout->nb_channels == 2) {
        AVChannelLayout assumed = {0};
        av_channel_layout_default(&assumed, layout->nb_channels);
        info->source_layout_mask = assumed.u.mask;
        info->source_layout_assumed = 1;
        av_channel_layout_uninit(&assumed);
    }
}

/* swresample only changes sample rate and sample representation. Its output retains the source
 * channel order, including UNSPEC multichannel layouts, for the Rust mixer to make the routing
 * decision from the exported source metadata. */
static int bn_audio_preserve_output_layout(BnAudioDecoder *decoder,
                                           const AVChannelLayout *input) {
    /* Stereo endpoints retain the former swr path byte for byte. Only multichannel
     * bindings opt into source-layout PCM for the worker's fixed mixer. */
    if (decoder->output_channels < 4) {
        av_channel_layout_default(&decoder->output_layout, decoder->output_channels);
        return 0;
    }
    const int result = av_channel_layout_copy(&decoder->output_layout, input);
    if (result < 0)
        return result;
    decoder->output_channels = input->nb_channels;
    return 0;
}

static int bn_audio_is_dsd(enum AVCodecID id) {
    return id == AV_CODEC_ID_DSD_LSBF_PLANAR || id == AV_CODEC_ID_DSD_MSBF ||
           id == AV_CODEC_ID_DST;
}

/* Opens the stream's FFmpeg decoder and takes its input rate. */
static int32_t bn_audio_open_codec(BnAudioDecoder *decoder, const AVStream *stream, char *error,
                                   size_t error_capacity) {
    const AVCodec *codec = avcodec_find_decoder(stream->codecpar->codec_id);
    if (!codec) {
        bn_error(error, error_capacity, "audio stream has no available decoder");
        return BN_AUDIO_CODEC;
    }
    decoder->codec = avcodec_alloc_context3(codec);
    if (!decoder->codec) {
        bn_error(error, error_capacity, "could not allocate the audio codec context");
        return BN_AUDIO_DECODER_ALLOCATION;
    }
    int result = avcodec_parameters_to_context(decoder->codec, stream->codecpar);
    if (result < 0) {
        bn_av_error(error, error_capacity, "could not copy audio codec parameters", result);
        return BN_AUDIO_CODEC_PARAMETERS;
    }
    decoder->codec->pkt_timebase = stream->time_base;
    result = avcodec_open2(decoder->codec, codec, NULL);
    if (result < 0) {
        bn_av_error(error, error_capacity, "could not open the audio codec", result);
        return BN_AUDIO_CODEC;
    }
    if (decoder->codec->sample_rate <= 0 || decoder->codec->ch_layout.nb_channels <= 0) {
        bn_error(error, error_capacity,
                 "audio codec exposed an invalid sample rate or channel layout");
        return BN_AUDIO_CODEC;
    }
    decoder->input_sample_rate = decoder->codec->sample_rate;
    return BN_AUDIO_OK;
}

/* Builds the resampler from the decoded input to f32 at the output rate, and places a first
 * frame with no timestamp of its own at the stream's start. A seek forgets that: the new run
 * needs its own frame or packet anchor. */
static int32_t bn_audio_open_resampler(BnAudioDecoder *decoder, const AVStream *stream,
                                       const AVChannelLayout *input_layout,
                                       enum AVSampleFormat input_format, char *error,
                                       size_t error_capacity) {
    int result = bn_audio_preserve_output_layout(decoder, input_layout);
    if (result >= 0)
        result = swr_alloc_set_opts2(&decoder->resampler, &decoder->output_layout,
                                     AV_SAMPLE_FMT_FLT, decoder->output_sample_rate, input_layout,
                                     input_format, decoder->input_sample_rate, 0, NULL);
    if (result < 0 || !decoder->resampler) {
        if (result < 0)
            bn_av_error(error, error_capacity, "could not allocate the audio resampler", result);
        else
            bn_error(error, error_capacity, "could not allocate the audio resampler");
        return BN_AUDIO_RESAMPLER;
    }
    result = swr_init(decoder->resampler);
    if (result < 0) {
        bn_av_error(error, error_capacity, "could not initialize the audio resampler", result);
        return BN_AUDIO_RESAMPLER;
    }
    decoder->next_output_sample_pts = av_rescale_q(
        stream->start_time != AV_NOPTS_VALUE ? stream->start_time : 0, stream->time_base,
        (AVRational) {1, decoder->output_sample_rate});
    decoder->has_next_output_sample_pts = 1;
    return BN_AUDIO_OK;
}

/* A DSD open only probes until its caller chooses raw bytes or PCM. A caller that reads PCM
 * gets FFmpeg's DSD-to-PCM decoder, opened here on its first seek or read. */
static int32_t bn_audio_ensure_dsd_pcm(BnAudioDecoder *decoder, char *error,
                                       size_t error_capacity) {
    if (!decoder || decoder->codec || !decoder->format || !decoder->packet ||
        decoder->audio_index < 0)
        return BN_AUDIO_OK;
    const AVStream *stream = decoder->format->streams[decoder->audio_index];
    if (!bn_audio_is_dsd(stream->codecpar->codec_id))
        return BN_AUDIO_OK;
    int32_t result = bn_audio_open_codec(decoder, stream, error, error_capacity);
    if (result != BN_AUDIO_OK)
        return result;
    decoder->frame = av_frame_alloc();
    if (!decoder->frame) {
        bn_error(error, error_capacity, "could not allocate reusable audio frame storage");
        return BN_AUDIO_FRAME_ALLOCATION;
    }
    return bn_audio_open_resampler(decoder, stream, &decoder->codec->ch_layout,
                                   decoder->codec->sample_fmt, error, error_capacity);
}

/* The stream's timing and rates, as the caller reads them. */
static void bn_audio_stream_info(const BnAudioDecoder *decoder, const AVStream *stream,
                                 int32_t source_channels, int32_t output_sample_rate,
                                 int32_t output_channels, BnAudioStreamInfo *out_info) {
    out_info->time_base_num = stream->time_base.num;
    out_info->time_base_den = stream->time_base.den;
    out_info->has_start_time = stream->start_time != AV_NOPTS_VALUE;
    out_info->start_time = out_info->has_start_time ? stream->start_time : 0;
    out_info->has_format_start_time = decoder->format->start_time != AV_NOPTS_VALUE;
    out_info->format_start_time_us =
        out_info->has_format_start_time ? decoder->format->start_time : 0;
    out_info->source_sample_rate = decoder->input_sample_rate;
    out_info->source_channels = source_channels;
    out_info->output_sample_rate = output_sample_rate;
    out_info->output_channels = output_channels;
    out_info->stream_index = decoder->audio_index;
}

/* Moves the reader to `from`: served from what the demux thread holds when that covers it,
 * otherwise a seek request to it. Returns FFmpeg's error, or 0. */
static int bn_audio_demux_seek(BnAudioDecoder *decoder, int64_t from) {
    BnDemux *demux = bn_audio_demux(decoder);
    int served = 0;
    if (bn_demux_ensure(demux) == 0) {
        AcquireSRWLockExclusive(&demux->lock);
        served = bn_demux_trim_to_locked(demux, from);
        ReleaseSRWLockExclusive(&demux->lock);
    }
    return served ? 0 : bn_demux_request_seek(demux, INT64_MIN, from, from, 0);
}

void bn_audio_decoder_set_source_local(BnAudioDecoder *decoder, int32_t local) {
    if (decoder)
        bn_demux_set_source_local(&decoder->demux, local);
}

void bn_audio_decoder_cancel(BnAudioDecoder *decoder) {
    if (decoder)
        bn_demux_cancel(&decoder->demux);
}

/* How much sound is queued ahead of the decoder, in microseconds, and whether the input ended. */
int32_t bn_audio_decoder_input_state(BnAudioDecoder *decoder, int64_t *out_queued_us,
                                     int32_t *out_ended) {
    if (!decoder || !out_queued_us || !out_ended)
        return BN_AUDIO_INVALID_ARGUMENT;
    *out_queued_us = 0;
    *out_ended = decoder->input_eof;
    BnDemux *demux = &decoder->demux;
    if (!demux->thread)
        return BN_AUDIO_OK;
    AcquireSRWLockExclusive(&demux->lock);
    *out_queued_us = bn_demux_queued_us_locked(demux);
    *out_ended = demux->eof || demux->error != 0;
    ReleaseSRWLockExclusive(&demux->lock);
    return BN_AUDIO_OK;
}

int64_t bn_audio_decoder_resampler_input_frames(const BnAudioDecoder *decoder) {
    return decoder ? decoder->resampler_input_frames : 0;
}

int64_t bn_audio_decoder_passed_over(const BnAudioDecoder *decoder) {
    return decoder ? decoder->passed_over : 0;
}

void bn_audio_decoder_destroy(BnAudioDecoder **decoder) {
    if (!decoder || !*decoder)
        return;
    BnAudioDecoder *value = *decoder;
    bn_demux_stop(&value->demux);
    bn_tak_destroy(value->tak);
    if (value->flac_parser)
        av_parser_close(value->flac_parser);
    free(value->dsd_bytes);
    free(value->output_samples);
    if (value->resampler)
        swr_free(&value->resampler);
    av_channel_layout_uninit(&value->output_layout);
    if (value->frame)
        av_frame_free(&value->frame);
    if (value->packet)
        av_packet_free(&value->packet);
    if (value->codec)
        avcodec_free_context(&value->codec);
    if (value->format)
        avformat_close_input(&value->format);
    bn_custom_io_free(&value->custom_io);
    free(value);
    *decoder = NULL;
}

static BnAudioDecoder *bn_audio_decoder_allocate(
    int32_t output_sample_rate, int32_t output_channels, char *error,
    size_t error_capacity) {
    BnAudioDecoder *decoder =
        (BnAudioDecoder *) calloc(1, sizeof(*decoder));
    if (!decoder) {
        bn_error(error, error_capacity,
                 "could not allocate the audio decoder handle");
        return NULL;
    }
    decoder->audio_index = -1;
    decoder->output_sample_rate = output_sample_rate;
    decoder->output_channels = output_channels;
    return decoder;
}

static int32_t bn_audio_decoder_finish_open(
    BnAudioDecoder *decoder, int32_t audio_stream_index,
    const wchar_t *path, const BnByteSource *source,
    int32_t output_sample_rate, int32_t output_channels,
    BnAudioDecoder **out_decoder, BnAudioStreamInfo *out_info, char *error,
    size_t error_capacity, int32_t *native_error) {
    int result = avformat_find_stream_info(decoder->format, NULL);
    if (bn_interrupt_expired(&decoder->interrupt))
        result = AVERROR_EXIT;
    if (result < 0) {
        if (native_error) *native_error = result;
        bn_av_error(error, error_capacity,
                    "could not read audio stream information", result);
        bn_audio_decoder_destroy(&decoder);
        return BN_AUDIO_STREAM_INFO;
    }

    if (audio_stream_index == -1) {
        decoder->audio_index =
            av_find_best_stream(decoder->format, AVMEDIA_TYPE_AUDIO, -1, -1,
                                NULL, 0);
        if (decoder->audio_index < 0) {
            bn_error(error, error_capacity,
                     "the local media has no decodable audio stream");
            bn_audio_decoder_destroy(&decoder);
            return BN_AUDIO_NO_STREAM;
        }
    } else {
        if (audio_stream_index < 0 ||
            (unsigned int) audio_stream_index >= decoder->format->nb_streams ||
            decoder->format->streams[audio_stream_index]
                    ->codecpar->codec_type != AVMEDIA_TYPE_AUDIO) {
            bn_error(error, error_capacity,
                     "stream index %d does not identify an audio stream",
                     audio_stream_index);
            bn_audio_decoder_destroy(&decoder);
            return BN_AUDIO_INVALID_STREAM;
        }
        decoder->audio_index = audio_stream_index;
    }
    AVStream *stream = decoder->format->streams[decoder->audio_index];
    if (stream->time_base.num <= 0 || stream->time_base.den <= 0) {
        bn_error(error, error_capacity,
                 "audio stream exposed an invalid time base %d/%d",
                 stream->time_base.num, stream->time_base.den);
        bn_audio_decoder_destroy(&decoder);
        return BN_AUDIO_INVALID_TIME_BASE;
    }

    /* DSD is packet bytes, not PCM. Keep the probed container and a packet for the caller that
     * explicitly enables the raw ABI; do not ask avcodec or swr to turn one-bit samples into PCM. */
    if (bn_audio_is_dsd(stream->codecpar->codec_id)) {
        const int channels = stream->codecpar->ch_layout.nb_channels;
        const int byte_rate = stream->codecpar->sample_rate;
        if (channels <= 0 || byte_rate <= 0) {
            bn_error(error, error_capacity, "DSD stream has an invalid channel count or bit rate");
            bn_audio_decoder_destroy(&decoder);
            return BN_AUDIO_CODEC_PARAMETERS;
        }
        decoder->packet = av_packet_alloc();
        if (!decoder->packet) {
            bn_error(error, error_capacity, "could not allocate reusable DSD packet storage");
            bn_audio_decoder_destroy(&decoder);
            return BN_AUDIO_PACKET_ALLOCATION;
        }
        decoder->input_sample_rate = byte_rate;
        decoder->codec_name = avcodec_get_name(stream->codecpar->codec_id);
        bn_audio_stream_info(decoder, stream, channels, output_sample_rate,
                             output_channels >= 4 ? channels : output_channels, out_info);
        bn_audio_source_layout_info(&stream->codecpar->ch_layout, out_info);
        *out_decoder = decoder;
        return BN_AUDIO_OK;
    }

    AVChannelLayout tak_layout = {0};
    enum AVSampleFormat input_format;
    AVChannelLayout *input_layout;
    if (stream->codecpar->codec_id == AV_CODEC_ID_TAK) {
        result = bn_tak_open(decoder, path, source, &tak_layout, error, error_capacity);
        if (result < 0) {
            if (native_error) *native_error = decoder->transport_error;
            av_channel_layout_uninit(&tak_layout);
            bn_audio_decoder_destroy(&decoder);
            return BN_AUDIO_CODEC;
        }
        input_format = AV_SAMPLE_FMT_S32;
        input_layout = &tak_layout;
    } else {
        result = bn_audio_open_codec(decoder, stream, error, error_capacity);
        if (result != BN_AUDIO_OK) {
            bn_audio_decoder_destroy(&decoder);
            return result;
        }
        input_format = decoder->codec->sample_fmt;
        input_layout = &decoder->codec->ch_layout;
    }
    if (stream->codecpar->codec_id == AV_CODEC_ID_FLAC &&
        strcmp(decoder->format->iformat->name, "ogg") == 0) {
        decoder->flac_parser = av_parser_init(AV_CODEC_ID_FLAC);
        if (!decoder->flac_parser || stream->codecpar->extradata_size < 34) {
            bn_error(error, error_capacity, "could not initialize Ogg FLAC frame timing");
            bn_audio_decoder_destroy(&decoder);
            return BN_AUDIO_CODEC_PARAMETERS;
        }
        decoder->flac_parser->flags = PARSER_FLAG_COMPLETE_FRAMES | PARSER_FLAG_USE_CODEC_TS;
        const uint8_t *info = stream->codecpar->extradata;
        decoder->flac_block_size = ((int) info[2] << 8) | info[3];
    }
    const int input_channels = input_layout->nb_channels;

    decoder->packet = av_packet_alloc();
    if (!decoder->packet) {
        bn_error(error, error_capacity,
                 "could not allocate reusable audio packet storage");
        bn_audio_decoder_destroy(&decoder);
        av_channel_layout_uninit(&tak_layout);
        return BN_AUDIO_PACKET_ALLOCATION;
    }
    decoder->frame = av_frame_alloc();
    if (!decoder->frame) {
        bn_error(error, error_capacity,
                 "could not allocate reusable audio frame storage");
        bn_audio_decoder_destroy(&decoder);
        av_channel_layout_uninit(&tak_layout);
        return BN_AUDIO_FRAME_ALLOCATION;
    }

    bn_audio_source_layout_info(input_layout, out_info);
    result = bn_audio_open_resampler(decoder, stream, input_layout, input_format, error,
                                     error_capacity);
    av_channel_layout_uninit(&tak_layout);
    if (result != BN_AUDIO_OK) {
        bn_audio_decoder_destroy(&decoder);
        return result;
    }
    decoder->codec_name = avcodec_get_name(stream->codecpar->codec_id);
    bn_audio_stream_info(decoder, stream, input_channels, output_sample_rate,
                         decoder->output_channels, out_info);
    *out_decoder = decoder;
    return BN_AUDIO_OK;
}

int32_t bn_audio_tracks_enumerate(const char *local_path,
                                  BnAudioTrackInfo *out_tracks,
                                  size_t track_capacity,
                                  size_t *out_track_count, char *error,
                                  size_t error_capacity) {
    if (!local_path || !out_track_count ||
        (!out_tracks && track_capacity != 0)) {
        bn_error(error, error_capacity,
                 "audio track enumeration received an invalid argument");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    *out_track_count = 0;

    AVFormatContext *format = NULL;
    int result = avformat_open_input(&format, local_path, NULL, NULL);
    if (result < 0) {
        bn_av_error(error, error_capacity,
                    "could not open input for audio track enumeration",
                    result);
        return BN_AUDIO_INPUT;
    }
    result = avformat_find_stream_info(format, NULL);
    if (result < 0) {
        bn_av_error(error, error_capacity,
                    "could not read audio track information", result);
        avformat_close_input(&format);
        return BN_AUDIO_STREAM_INFO;
    }

    size_t track_count = 0;
    for (unsigned int index = 0; index < format->nb_streams; ++index) {
        if (format->streams[index]->codecpar->codec_type ==
            AVMEDIA_TYPE_AUDIO)
            ++track_count;
    }
    *out_track_count = track_count;
    if (!out_tracks) {
        avformat_close_input(&format);
        return BN_AUDIO_OK;
    }
    if (track_capacity < track_count) {
        bn_error(error, error_capacity,
                 "audio track buffer capacity %zu is smaller than required "
                 "count %zu",
                 track_capacity, track_count);
        avformat_close_input(&format);
        return BN_AUDIO_BUFFER_TOO_SMALL;
    }

    size_t output_index = 0;
    for (unsigned int stream_index = 0; stream_index < format->nb_streams;
         ++stream_index) {
        AVStream *stream = format->streams[stream_index];
        if (stream->codecpar->codec_type != AVMEDIA_TYPE_AUDIO)
            continue;
        BnAudioTrackInfo *track = &out_tracks[output_index++];
        memset(track, 0, sizeof(*track));
        track->stream_index = (int32_t) stream_index;
        track->channels = stream->codecpar->ch_layout.nb_channels;
        const AVDictionaryEntry *language =
            av_dict_get(stream->metadata, "language", NULL, 0);
        bn_copy_text(track->language, sizeof(track->language),
                     language ? language->value : "");
        bn_copy_text(track->codec, sizeof(track->codec),
                     avcodec_get_name(stream->codecpar->codec_id));
    }

    avformat_close_input(&format);
    return BN_AUDIO_OK;
}

/* Opens a local path's default audio stream: the test batteries' entry; the player opens byte
 * sources. */
int32_t bn_audio_decoder_open(const char *local_path,
                              int32_t output_sample_rate,
                              int32_t output_channels,
                              BnAudioDecoder **out_decoder,
                              BnAudioStreamInfo *out_info, char *error,
                              size_t error_capacity) {
    if (!local_path || output_sample_rate <= 0 || output_channels <= 0 ||
        !out_decoder || !out_info) {
        bn_error(error, error_capacity,
                 "audio open received an invalid argument");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    *out_decoder = NULL;
    memset(out_info, 0, sizeof(*out_info));

    BnAudioDecoder *decoder = bn_audio_decoder_allocate(
        output_sample_rate, output_channels, error, error_capacity);
    if (!decoder)
        return BN_AUDIO_DECODER_ALLOCATION;

    const int result =
        avformat_open_input(&decoder->format, local_path, NULL, NULL);
    if (result < 0) {
        bn_av_error(error, error_capacity, "could not open audio input",
                    result);
        bn_audio_decoder_destroy(&decoder);
        return BN_AUDIO_INPUT;
    }
    int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, local_path, -1, NULL, 0);
    wchar_t *path = length > 0 ? malloc((size_t) length * sizeof(wchar_t)) : NULL;
    if (!path || !MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, local_path, -1, path, length)) {
        free(path);
        bn_error(error, error_capacity, "could not convert the audio input path to UTF-16");
        bn_audio_decoder_destroy(&decoder);
        return BN_AUDIO_INPUT;
    }
    int32_t status = bn_audio_decoder_finish_open(
        decoder, -1, path, NULL, output_sample_rate, output_channels,
        out_decoder, out_info, error, error_capacity, NULL);
    free(path);
    return status;
}

int32_t bn_audio_decoder_open_source(const BnByteSource *source,
                                     int32_t output_sample_rate,
                                     int32_t output_channels,
                                     BnAudioDecoder **out_decoder,
                                     BnAudioStreamInfo *out_info, char *error,
                                     size_t error_capacity) {
    return bn_audio_decoder_open_source_stream_with_error(
        source, -1, output_sample_rate, output_channels, out_decoder, out_info,
        error, error_capacity, NULL);
}

int32_t bn_audio_decoder_open_source_stream_with_error(
    const BnByteSource *source, int32_t audio_stream_index,
    int32_t output_sample_rate, int32_t output_channels,
    BnAudioDecoder **out_decoder, BnAudioStreamInfo *out_info, char *error,
    size_t error_capacity, int32_t *native_error) {
    if (native_error) *native_error = 0;
    if (!source || !source->opaque || !source->read || !source->seek ||
        audio_stream_index < -1 || output_sample_rate <= 0 ||
        output_channels <= 0 || !out_decoder || !out_info) {
        bn_error(error, error_capacity,
                 "source audio open received an invalid argument");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    *out_decoder = NULL;
    memset(out_info, 0, sizeof(*out_info));

    BnAudioDecoder *decoder = bn_audio_decoder_allocate(
        output_sample_rate, output_channels, error, error_capacity);
    if (!decoder)
        return BN_AUDIO_DECODER_ALLOCATION;

    const int result = bn_custom_input_open(
        &decoder->format, &decoder->custom_io, source, &decoder->interrupt);
    if (result < 0) {
        if (native_error) *native_error = result;
        bn_av_error(error, error_capacity,
                    "could not open the byte-source audio", result);
        bn_audio_decoder_destroy(&decoder);
        return BN_AUDIO_INPUT;
    }
    const int32_t status = bn_audio_decoder_finish_open(
        decoder, audio_stream_index, (const wchar_t *) source->local_path, source,
        output_sample_rate, output_channels, out_decoder, out_info, error, error_capacity,
        native_error);
    /* finish_open destroys decoder on failure; only touch the published handle on success. */
    if (status == BN_AUDIO_OK) {
        if (bn_interrupt_expired(&(*out_decoder)->interrupt)) {
            if (native_error) *native_error = AVERROR_EXIT;
            bn_error(error, error_capacity, "audio open cancelled or timed out");
            bn_audio_decoder_destroy(out_decoder);
            return BN_AUDIO_INPUT;
        }
        bn_interrupt_deadline(&(*out_decoder)->interrupt, 0);
    }
    return status;
}

/* Read on the decoder owner thread, after seek/decode returns. Never read demux state here. */
int32_t bn_audio_decoder_transport_error(const BnAudioDecoder *decoder) {
    return decoder ? decoder->transport_error : 0;
}

const char *bn_audio_decoder_codec_name(const BnAudioDecoder *decoder) {
    return decoder && decoder->codec_name ? decoder->codec_name : "unknown";
}

/* A tag of the decoded file by name (any case): the container's, else the audio stream's — Ogg
 * and Opus keep their comments on the stream. NULL when neither has it. The text is the
 * decoder's, alive until it closes; read on the opening thread before the worker owns it. */
const char *bn_audio_decoder_tag(const BnAudioDecoder *decoder, const char *name) {
    if (!decoder || !decoder->format || !name)
        return NULL;
    const AVDictionaryEntry *entry = av_dict_get(decoder->format->metadata, name, NULL, 0);
    if ((!entry || !entry->value[0]) && decoder->audio_index >= 0 &&
        decoder->audio_index < (int)decoder->format->nb_streams)
        entry = av_dict_get(decoder->format->streams[decoder->audio_index]->metadata, name,
                            NULL, 0);
    return entry && entry->value[0] ? entry->value : NULL;
}

static uint8_t bn_dsd_reverse_bits(uint8_t value) {
    value = (uint8_t) (((value & 0x55u) << 1) | ((value >> 1) & 0x55u));
    value = (uint8_t) (((value & 0x33u) << 2) | ((value >> 2) & 0x33u));
    return (uint8_t) ((value << 4) | (value >> 4));
}

static int bn_dsd_reserve(BnAudioDecoder *decoder, int64_t frames,
                          char *error, size_t capacity) {
    if (frames <= 0 || frames > INT32_MAX || decoder->dsd_channels <= 0 ||
        frames > INT_MAX / decoder->dsd_channels) {
        bn_error(error, capacity, "DSD packet has an invalid byte-frame count");
        return BN_AUDIO_CONVERT;
    }
    const int bytes = (int) frames * decoder->dsd_channels;
    if (bytes <= decoder->dsd_capacity)
        return BN_AUDIO_OK;
    uint8_t *resized = (uint8_t *) realloc(decoder->dsd_bytes, (size_t) bytes);
    if (!resized) {
        bn_error(error, capacity, "could not allocate DSD output storage");
        return BN_AUDIO_BUFFER_ALLOCATION;
    }
    decoder->dsd_bytes = resized;
    decoder->dsd_capacity = bytes;
    return BN_AUDIO_OK;
}

static void bn_dsd_set_timing(const BnAudioDecoder *decoder, int64_t byte_frame,
                              BnFrameTiming *timing) {
    const AVStream *stream = decoder->format->streams[decoder->audio_index];
    timing->pts = av_rescale_q(byte_frame, (AVRational) {1, decoder->dsd_byte_rate},
                               stream->time_base);
    timing->has_timestamp = 1;
    timing->used_best_effort_timestamp = 1;
}

int32_t bn_audio_decoder_enable_dsd(BnAudioDecoder *decoder,
                                    BnAudioStreamInfo *out_info, char *error,
                                    size_t error_capacity) {
    if (!decoder || !decoder->format || !decoder->packet ||
        !out_info || decoder->audio_index < 0 || decoder->dsd_decode_started) {
        bn_error(error, error_capacity, "DSD enable requires a probed decoder before its first read");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    const AVStream *stream = decoder->format->streams[decoder->audio_index];
    const enum AVCodecID codec = stream->codecpar->codec_id;
    if (codec == AV_CODEC_ID_DST) {
        bn_error(error, error_capacity, "DST is compressed DSD and has no raw DSD byte stream");
        return BN_AUDIO_CODEC;
    }
    if (codec != AV_CODEC_ID_DSD_LSBF_PLANAR && codec != AV_CODEC_ID_DSD_MSBF) {
        bn_error(error, error_capacity, "audio stream is not a supported raw DSD layout");
        return BN_AUDIO_CODEC;
    }
    const int channels = stream->codecpar->ch_layout.nb_channels;
    const int byte_rate = stream->codecpar->sample_rate;
    if (channels <= 0 || byte_rate <= 0) {
        bn_error(error, error_capacity, "raw DSD stream has an invalid channel count or bit rate");
        return BN_AUDIO_CODEC_PARAMETERS;
    }
    decoder->dsd_enabled = 1;
    decoder->dsd_lsbf_planar = codec == AV_CODEC_ID_DSD_LSBF_PLANAR;
    decoder->dsd_channels = channels;
    decoder->dsd_byte_rate = byte_rate;
    decoder->dsd_has_position = 0;
    decoder->dsd_emitted_bytes = 0;
    decoder->dsd_total_bytes = stream->duration == AV_NOPTS_VALUE ? -1
        : av_rescale_q(stream->duration, stream->time_base,
                       (AVRational) {1, decoder->dsd_byte_rate});
    decoder->input_sample_rate = decoder->dsd_byte_rate;
    decoder->output_sample_rate = decoder->dsd_byte_rate;
    decoder->output_channels = channels;
    bn_audio_stream_info(decoder, stream, channels, decoder->dsd_byte_rate, channels, out_info);
    bn_audio_source_layout_info(&stream->codecpar->ch_layout, out_info);
    return BN_AUDIO_OK;
}

int32_t bn_audio_decoder_read_dsd(BnAudioDecoder *decoder, BnDsdChunk *out_chunk,
                                  char *error, size_t error_capacity) {
    if (decoder) decoder->transport_error = 0;
    if (!decoder || !decoder->dsd_enabled || !decoder->format || !decoder->packet ||
        !out_chunk || decoder->audio_index < 0) {
        bn_error(error, error_capacity, "raw DSD read received an invalid decoder");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    memset(out_chunk, 0, sizeof(*out_chunk));
    decoder->dsd_decode_started = 1;
    for (;;) {
        const int result = bn_demux_take(bn_audio_demux(decoder), decoder->packet);
        if (result == 0)
            return BN_AUDIO_AGAIN;
        if (result < 0) {
            if (result == AVERROR_EOF) {
                decoder->input_eof = 1;
                return BN_AUDIO_EOF;
            }
            decoder->transport_error = result;
            bn_av_error(error, error_capacity, "could not read a raw DSD packet", result);
            return BN_AUDIO_READ;
        }
        if (decoder->packet->stream_index != decoder->audio_index) {
            av_packet_unref(decoder->packet);
            continue;
        }
        if (decoder->packet->size <= 0 || decoder->packet->size % decoder->dsd_channels != 0) {
            bn_error(error, error_capacity, "raw DSD packet is not whole byte-frames");
            av_packet_unref(decoder->packet);
            return BN_AUDIO_READ;
        }
        const int packet_frames = decoder->packet->size / decoder->dsd_channels;
        if (!decoder->dsd_has_position) {
            const int64_t packet_pts = decoder->packet->pts != AV_NOPTS_VALUE
                                           ? decoder->packet->pts : decoder->packet->dts;
            decoder->dsd_emitted_bytes = packet_pts == AV_NOPTS_VALUE ? 0
                : av_rescale_q(packet_pts,
                               decoder->format->streams[decoder->audio_index]->time_base,
                               (AVRational) {1, decoder->dsd_byte_rate});
            decoder->dsd_has_position = 1;
        }
        int64_t frames = packet_frames;
        if (decoder->dsd_total_bytes >= 0) {
            const int64_t remaining = decoder->dsd_total_bytes - decoder->dsd_emitted_bytes;
            if (remaining <= 0) {
                av_packet_unref(decoder->packet);
                continue;
            }
            frames = FFMIN(frames, remaining);
        }
        const int reserved = bn_dsd_reserve(decoder, frames, error, error_capacity);
        if (reserved != BN_AUDIO_OK) {
            av_packet_unref(decoder->packet);
            return reserved;
        }
        if (decoder->dsd_lsbf_planar) {
            for (int frame = 0; frame < frames; ++frame)
                for (int channel = 0; channel < decoder->dsd_channels; ++channel)
                    decoder->dsd_bytes[frame * decoder->dsd_channels + channel] =
                        bn_dsd_reverse_bits(decoder->packet->data[channel * packet_frames + frame]);
        } else {
            memcpy(decoder->dsd_bytes, decoder->packet->data,
                   (size_t) frames * decoder->dsd_channels);
        }
        bn_dsd_set_timing(decoder, decoder->dsd_emitted_bytes, &out_chunk->timing);
        decoder->dsd_emitted_bytes += frames;
        out_chunk->bytes = decoder->dsd_bytes;
        out_chunk->frames = (int32_t) frames;
        av_packet_unref(decoder->packet);
        return BN_AUDIO_OK;
    }
}

int32_t bn_audio_decoder_seek(BnAudioDecoder *decoder, int64_t target_pts,
                              char *error, size_t error_capacity) {
    if (decoder) decoder->transport_error = 0;
    if (decoder && decoder->dsd_enabled) {
        if (!decoder->format || !decoder->packet || decoder->audio_index < 0) {
            bn_error(error, error_capacity, "raw DSD seek received an invalid decoder");
            return BN_AUDIO_INVALID_ARGUMENT;
        }
        av_packet_unref(decoder->packet);
        const int result = bn_audio_demux_seek(decoder, target_pts);
        if (result < 0) {
            decoder->transport_error = result;
            bn_av_error(error, error_capacity, "could not start the raw DSD reader seek", result);
            return BN_AUDIO_SEEK;
        }
        decoder->input_eof = 0;
        decoder->dsd_has_position = 0;
        decoder->dsd_emitted_bytes = 0;
        return BN_AUDIO_OK;
    }
    const int32_t prepared = bn_audio_ensure_dsd_pcm(decoder, error, error_capacity);
    if (prepared != BN_AUDIO_OK)
        return prepared;
    if (!decoder || !decoder->format || (!decoder->codec && !decoder->tak) || !decoder->packet ||
        !decoder->frame || !decoder->resampler || decoder->audio_index < 0) {
        bn_error(error, error_capacity,
                 "audio seek received an invalid decoder");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    av_packet_unref(decoder->packet);
    av_frame_unref(decoder->frame);
    if (decoder->tak) {
        if (bn_tak_seek(decoder, target_pts, error, error_capacity) < 0)
            return BN_AUDIO_SEEK;
    } else {
        /* A flushed decoder needs its declared history before the requested sample. WMA drops
         * two frames while rebuilding its overlap; starting at the target loses those samples.
         * The pump still trims decoded PCM to the original target. */
        const AVStream *stream = decoder->format->streams[decoder->audio_index];
        const int history = FFMAX(decoder->codec->delay, decoder->codec->seek_preroll);
        const int64_t preroll = av_rescale_q_rnd(history,
            (AVRational) {1, decoder->input_sample_rate}, stream->time_base, AV_ROUND_UP);
        const int64_t start = stream->start_time != AV_NOPTS_VALUE ? stream->start_time : 0;
        /* WMA Lossless neither reports its discarded samples while waiting for a seekable tile
         * nor preserves the originating packet's timestamp through its bit reservoir. Decode
         * from the origin so the sample count locates every frame, including after a seek. */
        const int64_t decode_from = decoder->codec->codec_id != AV_CODEC_ID_WMALOSSLESS &&
                                    target_pts > start && target_pts - start > preroll
                                        ? target_pts - preroll : start;
        const int result = bn_audio_demux_seek(decoder, decode_from);
        if (result < 0) {
            decoder->transport_error = result;
            bn_av_error(error, error_capacity, "could not start the audio container reader",
                        result);
            return BN_AUDIO_SEEK;
        }
        avcodec_flush_buffers(decoder->codec);
    }
    swr_close(decoder->resampler);
    const int resampler_result = swr_init(decoder->resampler);
    if (resampler_result < 0) {
        bn_av_error(error, error_capacity,
                    "could not reset the audio resampler", resampler_result);
        return BN_AUDIO_RESAMPLER;
    }
    decoder->input_eof = 0;
    decoder->resampler_drained = 0;
    decoder->has_next_output_sample_pts = 0;
    decoder->next_output_sample_pts = 0;
    const AVStream *seek_stream = decoder->format->streams[decoder->audio_index];
    decoder->seek_target_sample_pts = av_rescale_q(target_pts, seek_stream->time_base,
        (AVRational) {1, decoder->output_sample_rate});
    decoder->has_seek_gap_base = 0;
    decoder->has_packet_sample_pts = 0;
    /* What was passed over before the seek is not before where it lands. */
    decoder->run_passed_over = 0;
    decoder->gap_open = 0;
    decoder->silence_owed = 0;
    decoder->frame_held = 0;
    if (decoder->codec && decoder->codec->codec_id == AV_CODEC_ID_WMALOSSLESS) {
        decoder->next_output_sample_pts = av_rescale_q(
            seek_stream->start_time != AV_NOPTS_VALUE ? seek_stream->start_time : 0,
            seek_stream->time_base, (AVRational) {1, decoder->output_sample_rate});
        decoder->has_next_output_sample_pts = 1;
    }
    return BN_AUDIO_OK;
}

static int32_t bn_audio_reserve_output(BnAudioDecoder *decoder,
                                       int64_t maximum_frames, char *error,
                                       size_t error_capacity) {
    if (maximum_frames <= 0 || maximum_frames > INT32_MAX) {
        bn_error(error, error_capacity,
                 "audio resampler requested an invalid output size");
        return BN_AUDIO_CONVERT;
    }
    if (maximum_frames <= decoder->output_capacity_frames)
        return BN_AUDIO_OK;
    const size_t frames = (size_t) maximum_frames;
    const size_t channels = (size_t) decoder->output_channels;
    if (frames > SIZE_MAX / channels ||
        frames * channels > SIZE_MAX / sizeof(float)) {
        bn_error(error, error_capacity, "audio output buffer size overflowed");
        return BN_AUDIO_BUFFER_ALLOCATION;
    }
    float *resized = (float *) realloc(
        decoder->output_samples, frames * channels * sizeof(float));
    if (!resized) {
        bn_error(error, error_capacity,
                 "could not grow the audio output buffer");
        return BN_AUDIO_BUFFER_ALLOCATION;
    }
    decoder->output_samples = resized;
    decoder->output_capacity_frames = (int) maximum_frames;
    return BN_AUDIO_OK;
}

static void bn_audio_set_output_timing(BnAudioDecoder *decoder,
                                       int64_t output_sample_pts,
                                       BnFrameTiming *timing) {
    const AVStream *stream = decoder->format->streams[decoder->audio_index];
    timing->pts = av_rescale_q(output_sample_pts,
                               (AVRational) {1, decoder->output_sample_rate},
                               stream->time_base);
    timing->has_timestamp = 1;
    timing->used_best_effort_timestamp = 1;
}

static int32_t bn_audio_drain_resampler(BnAudioDecoder *decoder,
                                        BnAudioChunk *out_chunk, char *error,
                                        size_t error_capacity) {
    if (decoder->resampler_drained)
        return BN_AUDIO_EOF;
    int maximum_frames = swr_get_out_samples(decoder->resampler, 0);
    if (maximum_frames < 0) {
        bn_av_error(error, error_capacity,
                    "could not query delayed resampler output", maximum_frames);
        return BN_AUDIO_CONVERT;
    }
    if (maximum_frames == 0) {
        decoder->resampler_drained = 1;
        return BN_AUDIO_EOF;
    }
    const int32_t reserve = bn_audio_reserve_output(
        decoder, maximum_frames, error, error_capacity);
    if (reserve != BN_AUDIO_OK)
        return reserve;
    uint8_t *output_planes[1] = {(uint8_t *) decoder->output_samples};
    const int converted = swr_convert(decoder->resampler, output_planes,
                                      maximum_frames, NULL, 0);
    if (converted < 0) {
        bn_av_error(error, error_capacity,
                    "could not drain delayed resampler output", converted);
        return BN_AUDIO_CONVERT;
    }
    if (converted == 0) {
        decoder->resampler_drained = 1;
        return BN_AUDIO_EOF;
    }
    out_chunk->samples = decoder->output_samples;
    out_chunk->frames = converted;
    if (decoder->has_next_output_sample_pts) {
        bn_audio_set_output_timing(decoder, decoder->next_output_sample_pts,
                                   &out_chunk->timing);
        decoder->next_output_sample_pts += converted;
    }
    return BN_AUDIO_OK;
}

/* Silence is handed out this many output frames at a time: what a long run of frames that did not
 * decode is owed never grows the output buffer past an ordinary chunk. */
#define BN_AUDIO_SILENCE_BLOCK 4096

/* A codec frame that did not decode. FFmpeg has already dropped it, and every FLAC frame (as every
 * frame of the codecs that report this) decodes on its own, so the stream goes on from the next
 * one. It is counted, and the next frame with a place of its own says how much went missing. */
static void bn_audio_pass_over(BnAudioDecoder *decoder) {
    decoder->passed_over += 1;
    decoder->run_passed_over = 1;
    decoder->gap_open = 1;
    if (!decoder->has_next_output_sample_pts && !decoder->has_seek_gap_base) {
        decoder->seek_gap_base_sample_pts = decoder->has_packet_sample_pts
            ? FFMIN(decoder->seek_target_sample_pts, decoder->packet_sample_pts)
            : decoder->seek_target_sample_pts;
        decoder->has_seek_gap_base = 1;
    }
}

/* Reconcile the first stamped frame after a failed packet with either the last output block or
 * the separate seek gap base. A positive return means silence must precede the held frame. */
static int bn_audio_account_gap(BnAudioDecoder *decoder, int64_t *output_sample_pts) {
    if (!decoder->run_passed_over ||
        (!decoder->has_next_output_sample_pts && !decoder->has_seek_gap_base))
        return 0;
    const int64_t ended = decoder->has_next_output_sample_pts
        ? decoder->next_output_sample_pts : decoder->seek_gap_base_sample_pts;
    decoder->has_seek_gap_base = 0;
    if (decoder->gap_open && *output_sample_pts > ended) {
        decoder->gap_open = 0;
        decoder->silence_owed = *output_sample_pts - ended;
        decoder->next_output_sample_pts = ended;
        decoder->has_next_output_sample_pts = 1;
        return 1;
    }
    decoder->gap_open = 0;
    /* FFmpeg's FLAC parser can miscount around a corrupt frame. Output never steps back. */
    *output_sample_pts = FFMAX(*output_sample_pts, ended);
    return 0;
}

/* Native state probe for the seek/corruption regression: the real packet and frame path calls
 * these same transitions. It needs no device, demux thread or nondeterministic seek index. */
int32_t bn_audio_decoder_seek_gap_probe(int64_t target, int64_t bad_packet,
                                        int64_t next_frame, int64_t *out_start,
                                        int64_t *out_frames) {
    if (!out_start || !out_frames)
        return BN_AUDIO_INVALID_ARGUMENT;
    BnAudioDecoder decoder = {0};
    decoder.seek_target_sample_pts = target;
    decoder.has_packet_sample_pts = bad_packet != AV_NOPTS_VALUE;
    decoder.packet_sample_pts = bad_packet;
    decoder.next_output_sample_pts = 42; /* stale pre-seek value, deliberately unavailable */
    bn_audio_pass_over(&decoder);
    if (!bn_audio_account_gap(&decoder, &next_frame))
        return BN_AUDIO_CONVERT;
    *out_start = decoder.next_output_sample_pts;
    *out_frames = decoder.silence_owed;
    return BN_AUDIO_OK;
}

/* The next block of the silence owed, standing where the last output ended. */
static int32_t bn_audio_pay_silence(BnAudioDecoder *decoder, BnAudioChunk *out_chunk,
                                    char *error, size_t error_capacity) {
    const int64_t frames = FFMIN(decoder->silence_owed, BN_AUDIO_SILENCE_BLOCK);
    const int32_t reserve = bn_audio_reserve_output(decoder, frames, error, error_capacity);
    if (reserve != BN_AUDIO_OK)
        return reserve;
    memset(decoder->output_samples, 0,
           (size_t) frames * (size_t) decoder->output_channels * sizeof(float));
    bn_audio_set_output_timing(decoder, decoder->next_output_sample_pts, &out_chunk->timing);
    decoder->next_output_sample_pts += frames;
    decoder->silence_owed -= frames;
    out_chunk->samples = decoder->output_samples;
    out_chunk->frames = (int32_t) frames;
    return BN_AUDIO_OK;
}

/* Where the frame in hand comes out of the resampler, in output samples. A frame with no place of
 * its own answers `*out_has` 0: it continues the output before it. */
static int32_t bn_audio_frame_output_pts(BnAudioDecoder *decoder, int64_t *out_pts,
                                         int *out_has, char *error, size_t error_capacity) {
    const int input_rate = decoder->input_sample_rate;
    int64_t frame_pts = decoder->frame->best_effort_timestamp;
    if (frame_pts == AV_NOPTS_VALUE)
        frame_pts = decoder->frame->pts;
    /* WMA Lossless keeps a packet's frames in its bit reservoir until the next packet.
     * Its frame stamps then belong to that next packet, not to the PCM being returned.
     * Every run begins at the stream origin; decoded sample counts carry its clock. */
    if (decoder->codec && decoder->codec->codec_id == AV_CODEC_ID_WMALOSSLESS)
        frame_pts = AV_NOPTS_VALUE;
    *out_pts = 0;
    *out_has = 0;
    if (frame_pts == AV_NOPTS_VALUE)
        return BN_AUDIO_OK;
    const AVStream *stream = decoder->format->streams[decoder->audio_index];
    const int64_t input_sample_pts = av_rescale_q(frame_pts, stream->time_base,
                                                  (AVRational) {1, input_rate});
    if ((input_sample_pts > 0 && input_sample_pts > INT64_MAX / decoder->output_sample_rate) ||
        (input_sample_pts < 0 && input_sample_pts < INT64_MIN / decoder->output_sample_rate)) {
        bn_error(error, error_capacity, "audio resampler input timestamp overflowed");
        return BN_AUDIO_CONVERT;
    }
    const int64_t resampler_pts = swr_next_pts(decoder->resampler,
                                               input_sample_pts * decoder->output_sample_rate);
    *out_pts = av_rescale_rnd(resampler_pts, 1, input_rate,
                              AV_ROUND_NEAR_INF | AV_ROUND_PASS_MINMAX);
    *out_has = *out_pts != INT64_MIN;
    return BN_AUDIO_OK;
}

/* Resamples the frame in hand into `out_chunk`, standing at `output_sample_pts` when it has a place
 * of its own and where the last output ended when it has not. Answers how many frames came out —
 * none while the resampler still holds them — or a negative status. */
static int bn_audio_convert_frame(BnAudioDecoder *decoder, BnAudioChunk *out_chunk,
                                  int has_output_sample_pts, int64_t output_sample_pts,
                                  char *error, size_t error_capacity) {
    const int input_rate = decoder->input_sample_rate;
    const int64_t maximum_frames = av_rescale_rnd(
        swr_get_delay(decoder->resampler, input_rate) + decoder->frame->nb_samples,
        decoder->output_sample_rate, input_rate, AV_ROUND_UP);
    const int32_t reserve = bn_audio_reserve_output(decoder, maximum_frames, error,
                                                    error_capacity);
    if (reserve != BN_AUDIO_OK)
        return reserve;
    uint8_t *output_planes[1] = {
        (uint8_t *) decoder->output_samples,
    };
    const int converted = swr_convert(decoder->resampler, output_planes, (int) maximum_frames,
                                      (const uint8_t **) decoder->frame->extended_data,
                                      decoder->frame->nb_samples);
    if (converted < 0) {
        bn_av_error(error, error_capacity, "could not resample an audio frame", converted);
        return BN_AUDIO_CONVERT;
    }
    decoder->resampler_input_frames += decoder->frame->nb_samples;
    if (converted == 0)
        return 0;
    if (has_output_sample_pts) {
        bn_audio_set_output_timing(decoder, output_sample_pts, &out_chunk->timing);
        decoder->next_output_sample_pts = output_sample_pts + converted;
        decoder->has_next_output_sample_pts = 1;
    } else if (decoder->has_next_output_sample_pts) {
        /* A frame with no timestamp continues the one before it: a codec that splits
           one packet into several frames (APE, WMA) stamps only the packet's first. */
        bn_audio_set_output_timing(decoder, decoder->next_output_sample_pts,
                                   &out_chunk->timing);
        decoder->next_output_sample_pts += converted;
    }
    out_chunk->samples = decoder->output_samples;
    out_chunk->frames = converted;
    return converted;
}

int32_t bn_audio_decoder_decode_next(BnAudioDecoder *decoder,
                                     BnAudioChunk *out_chunk, char *error,
                                     size_t error_capacity) {
    if (decoder) decoder->transport_error = 0;
    if (decoder && decoder->dsd_enabled) {
        bn_error(error, error_capacity, "raw DSD is read through bn_audio_decoder_read_dsd");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    const int32_t prepared = bn_audio_ensure_dsd_pcm(decoder, error, error_capacity);
    if (prepared != BN_AUDIO_OK)
        return prepared;
    if (!decoder || (!decoder->codec && !decoder->tak) || !decoder->packet || !decoder->frame ||
        !decoder->resampler || !out_chunk) {
        bn_error(error, error_capacity,
                 "audio decode received an invalid argument");
        return BN_AUDIO_INVALID_ARGUMENT;
    }
    memset(out_chunk, 0, sizeof(*out_chunk));
    /* What was passed over is paid in silence before the frame that measured it comes out. */
    if (decoder->silence_owed > 0)
        return bn_audio_pay_silence(decoder, out_chunk, error, error_capacity);
    if (decoder->frame_held) {
        decoder->frame_held = 0;
        const int converted = bn_audio_convert_frame(decoder, out_chunk, 1,
                                                     decoder->held_output_pts, error,
                                                     error_capacity);
        if (converted != 0)
            return converted < 0 ? converted : BN_AUDIO_OK;
    }
    av_frame_unref(decoder->frame);

    for (;;) {
        int result = decoder->tak ? bn_tak_receive(decoder, error, error_capacity)
                                  : avcodec_receive_frame(decoder->codec, decoder->frame);
        if (result == 0) {
            int64_t output_sample_pts = 0;
            int has_output_sample_pts = 0;
            const int32_t placed = bn_audio_frame_output_pts(
                decoder, &output_sample_pts, &has_output_sample_pts, error, error_capacity);
            if (placed != BN_AUDIO_OK)
                return placed;
            if (has_output_sample_pts && bn_audio_account_gap(decoder, &output_sample_pts)) {
                decoder->frame_held = 1;
                decoder->held_output_pts = output_sample_pts;
                return bn_audio_pay_silence(decoder, out_chunk, error, error_capacity);
            }
            const int converted = bn_audio_convert_frame(decoder, out_chunk,
                                                         has_output_sample_pts,
                                                         output_sample_pts, error,
                                                         error_capacity);
            if (converted == 0) {
                av_frame_unref(decoder->frame);
                continue;
            }
            return converted < 0 ? converted : BN_AUDIO_OK;
        }
        if (result == AVERROR_EOF)
            return bn_audio_drain_resampler(decoder, out_chunk, error,
                                            error_capacity);
        if (decoder->tak)
            return BN_AUDIO_RECEIVE;
        if (result == AVERROR_INVALIDDATA) {
            bn_audio_pass_over(decoder);
            continue;
        }
        if (result != AVERROR(EAGAIN)) {
            bn_av_error(error, error_capacity,
                        "could not receive an audio frame", result);
            return BN_AUDIO_RECEIVE;
        }
        if (decoder->input_eof) {
            bn_error(error, error_capacity,
                     "audio decoder requested input after its drain packet");
            return BN_AUDIO_RECEIVE;
        }

        for (;;) {
            result = bn_demux_take(bn_audio_demux(decoder), decoder->packet);
            if (result == 0)
                return BN_AUDIO_AGAIN;   /* nothing arrived in this slice: not an error */
            if (result > 0)
                result = 0;
            if (result < 0) {
                if (result != AVERROR_EOF) {
                    decoder->transport_error = result;
                    bn_av_error(error, error_capacity,
                                "could not read an audio packet", result);
                    return BN_AUDIO_READ;
                }
                result = avcodec_send_packet(decoder->codec, NULL);
                decoder->input_eof = 1;
                if (result < 0 && result != AVERROR_EOF) {
                    bn_av_error(error, error_capacity,
                                "could not send the audio drain packet",
                                result);
                    return BN_AUDIO_SEND;
                }
                break;
            }

            const int is_audio =
                decoder->packet->stream_index == decoder->audio_index;
            if (is_audio && decoder->flac_parser) {
                /* Ogg's index points at pages, not at individual FLAC frames. After a seek its
                 * inferred packet stamps can begin partway through the page although the bytes
                 * begin at its first frame. FLAC's own frame number gives the sample position. */
                AVCodecParserContext *parser = decoder->flac_parser;
                uint8_t *data;
                int size;
                parser->pts = AV_NOPTS_VALUE;
                parser->duration = 0;
                result = av_parser_parse2(parser, decoder->codec, &data, &size,
                    decoder->packet->data, decoder->packet->size, AV_NOPTS_VALUE,
                    AV_NOPTS_VALUE, decoder->packet->pos);
                if (result < 0 || parser->pts == AV_NOPTS_VALUE || parser->duration <= 0) {
                    bn_error(error, error_capacity, "could not read Ogg FLAC frame timing");
                    return BN_AUDIO_READ;
                }
                int64_t sample = parser->pts;
                /* The last fixed-size frame may be short. Its frame number still counts full
                 * blocks; the parser multiplies it by this frame's length instead. */
                if (!(decoder->packet->data[1] & 1))
                    sample = (sample / parser->duration) * decoder->flac_block_size;
                const AVStream *stream = decoder->format->streams[decoder->audio_index];
                decoder->packet->pts = decoder->packet->dts = av_rescale_q(sample,
                    (AVRational) {1, decoder->input_sample_rate}, stream->time_base);
            }
            result = is_audio
                         ? avcodec_send_packet(decoder->codec,
                                               decoder->packet)
                         : 0;
            if (is_audio) {
                const AVStream *stream = decoder->format->streams[decoder->audio_index];
                const int64_t packet_pts = decoder->packet->pts != AV_NOPTS_VALUE
                    ? decoder->packet->pts : decoder->packet->dts;
                decoder->has_packet_sample_pts = packet_pts != AV_NOPTS_VALUE;
                if (decoder->has_packet_sample_pts)
                    decoder->packet_sample_pts = av_rescale_q(packet_pts, stream->time_base,
                        (AVRational) {1, decoder->output_sample_rate});
            }
            av_packet_unref(decoder->packet);
            if (!is_audio)
                continue;
            /* The packet was taken; the frame it decoded to straight away was not a frame. */
            if (result == AVERROR_INVALIDDATA) {
                bn_audio_pass_over(decoder);
                break;
            }
            if (result < 0 && result != AVERROR(EAGAIN)) {
                bn_av_error(error, error_capacity,
                            "audio decoder rejected a packet", result);
                return BN_AUDIO_SEND;
            }
            break;
        }
    }
}
