/* Single-threaded, fixed surround graph. Input is stereo FLTP; source LFE stays in
 * the worker's time-aligned bypass and never enters the spectral engine. */
#include <libavfilter/avfilter.h>
#include <libavfilter/buffersrc.h>
#include <libavfilter/buffersink.h>
#include <libavutil/channel_layout.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/mem.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef struct BnUpmix {
    AVFilterGraph *graph;
    AVFilterContext *source, *sink;
    AVFrame *frame;
    int64_t pts;
    int rate;
} BnUpmix;

void bn_upmix_free(BnUpmix **value) {
    if (!value || !*value) return;
    av_frame_free(&(*value)->frame);
    avfilter_graph_free(&(*value)->graph);
    av_freep(value);
}

int bn_upmix_open(uint64_t output_mask, int rate, BnUpmix **out) {
    BnUpmix *value = av_mallocz(sizeof(*value));
    if (!value) return AVERROR(ENOMEM);
    value->graph = avfilter_graph_alloc();
    value->frame = av_frame_alloc();
    value->rate = rate;
    if (!value->graph || !value->frame) { bn_upmix_free(&value); return AVERROR(ENOMEM); }
    value->graph->nb_threads = 1;
    AVFilterContext *surround = NULL;
    char args[512];
    snprintf(args, sizeof(args), "time_base=1/%d:sample_rate=%d:sample_fmt=fltp:channel_layout=stereo", rate, rate);
    int result = avfilter_graph_create_filter(&value->source, avfilter_get_by_name("abuffer"), "input", args, NULL, value->graph);
    snprintf(args, sizeof(args), "chl_in=stereo:chl_out=0x%llx:lfe=0:win_size=4096:overlap=0.5:win_func=hann:fc_out=0.7071:level_in=1:level_out=1:angle=90:focus=0:smooth=0.3", (unsigned long long)output_mask);
    if (result >= 0) result = avfilter_graph_create_filter(&surround, avfilter_get_by_name("surround"), "upmix", args, NULL, value->graph);
    if (result >= 0) result = avfilter_graph_create_filter(&value->sink, avfilter_get_by_name("abuffersink"), "output", NULL, NULL, value->graph);
    if (result >= 0) result = avfilter_link(value->source, 0, surround, 0);
    if (result >= 0) result = avfilter_link(surround, 0, value->sink, 0);
    if (result >= 0) result = avfilter_graph_config(value->graph, NULL);
    if (result < 0) { bn_upmix_free(&value); return result; }
    *out = value;
    return 0;
}

int bn_upmix_push(BnUpmix *value, const float *samples, int frames) {
    if (!samples) return av_buffersrc_add_frame_flags(value->source, NULL, 0);
    AVFrame *frame = value->frame;
    av_frame_unref(frame);
    frame->format = AV_SAMPLE_FMT_FLTP;
    frame->sample_rate = value->rate;
    frame->nb_samples = frames;
    frame->pts = value->pts;
    av_channel_layout_default(&frame->ch_layout, 2);
    int result = av_frame_get_buffer(frame, 0);
    if (result < 0) return result;
    for (int c = 0; c < 2; ++c)
        for (int i = 0; i < frames; ++i)
            ((float *)frame->extended_data[c])[i] = samples[2*i+c];
    value->pts += frames;
    return av_buffersrc_add_frame_flags(value->source, frame, 0);
}

/* Borrowed planes remain valid until the next call to this handle. Rust copies them
 * before pushing/pulling again. 1 = temporarily empty or EOF, negatives = failure. */
int bn_upmix_pull(BnUpmix *value, const float *const **planes, int *frames, int *channels) {
    av_frame_unref(value->frame);
    int result = av_buffersink_get_frame(value->sink, value->frame);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) return 1;
    if (result < 0) return result;
    if (value->frame->format != AV_SAMPLE_FMT_FLTP) return AVERROR(EINVAL);
    *planes = (const float *const *)value->frame->extended_data;
    *frames = value->frame->nb_samples;
    *channels = value->frame->ch_layout.nb_channels;
    return 0;
}
