/* Native ASIO output uses the same loaded driver and ASIO SDK as asio-sys. */
#include "asio.h"
#include <atomic>
#include <new>

using BnAsioCallback = void (*)(void *, unsigned char *const *, unsigned int);
using BnAsioReset = void (*)(void *);

struct BnNativeAsioWriter {
    ASIOBufferInfo *infos;
    unsigned char **current;
    long channels;
    long frames;
    void *opaque;
    BnAsioCallback callback;
    BnAsioReset reset;
    bool started;
};

static std::atomic<BnNativeAsioWriter *> active_writer { nullptr };

static void render_buffer(BnNativeAsioWriter *writer, long index) {
    for (long channel = 0; channel < writer->channels; ++channel)
        writer->current[channel] = static_cast<unsigned char *>(writer->infos[channel].buffers[index]);
    writer->callback(writer->opaque, writer->current, static_cast<unsigned int>(writer->frames));
}

static void request_reset() {
    BnNativeAsioWriter *writer = active_writer.load(std::memory_order_acquire);
    if (writer)
        writer->reset(writer->opaque);
}

static void buffer_switch(long index, ASIOBool) {
    BnNativeAsioWriter *writer = active_writer.load(std::memory_order_acquire);
    if (!writer)
        return;
    if (index < 0 || index > 1) {
        request_reset();
        return;
    }
    render_buffer(writer, index);
    ASIOOutputReady();
}

static void sample_rate_changed(ASIOSampleRate) { request_reset(); }

static long asio_message(long selector, long value, void *, double *) {
    if (selector == kAsioSelectorSupported) {
        switch (value) {
        case kAsioEngineVersion:
        case kAsioResetRequest:
        case kAsioBufferSizeChange:
        case kAsioResyncRequest:
        case kAsioLatenciesChanged:
        case kAsioSupportsTimeInfo:
            return 1;
        default:
            return 0;
        }
    }
    switch (selector) {
    case kAsioEngineVersion:
        return 2;
    case kAsioSupportsTimeInfo:
        return 1;
    case kAsioResetRequest:
    case kAsioBufferSizeChange:
    case kAsioResyncRequest:
    case kAsioLatenciesChanged:
        request_reset();
        return 1;
    default:
        return 0;
    }
}

static ASIOTime *buffer_switch_time_info(ASIOTime *time, long index, ASIOBool direct) {
    buffer_switch(index, direct);
    return time;
}

static ASIOCallbacks callbacks = { buffer_switch, sample_rate_changed, asio_message, buffer_switch_time_info };

extern "C" int bn_asio_set_dsd_io_format(void) {
    ASIOIoFormat format = {};
    format.FormatType = kASIODSDFormat;
    return ASIOFuture(kAsioSetIoFormat, &format) == ASE_SUCCESS && format.FormatType == kASIODSDFormat ? 0 : -1;
}

extern "C" int bn_asio_restore_pcm_io_format(void) {
    ASIOIoFormat format = {};
    format.FormatType = kASIOPCMFormat;
    return ASIOFuture(kAsioSetIoFormat, &format) == ASE_SUCCESS && format.FormatType == kASIOPCMFormat ? 0 : -1;
}

extern "C" int bn_asio_output_sample_type(long channel, int *type) {
    ASIOChannelInfo info = {};
    info.channel = channel;
    info.isInput = ASIOFalse;
    if (ASIOGetChannelInfo(&info) != ASE_OK)
        return -1;
    *type = static_cast<int>(info.type);
    return 0;
}

extern "C" int bn_asio_preferred_buffer_frames(unsigned int *frames) {
    long minimum = 0, maximum = 0, preferred = 0, granularity = 0;
    if (ASIOGetBufferSize(&minimum, &maximum, &preferred, &granularity) != ASE_OK || preferred <= 0)
        return -1;
    *frames = static_cast<unsigned int>(preferred);
    return 0;
}

extern "C" int bn_asio_output_latency_frames(unsigned int *frames) {
    long input = 0, output = 0;
    if (ASIOGetLatencies(&input, &output) != ASE_OK || output < 0)
        return -1;
    *frames = static_cast<unsigned int>(output);
    return 0;
}

static void free_writer(BnNativeAsioWriter *writer) {
    delete[] writer->current;
    delete[] writer->infos;
    delete writer;
}

extern "C" BnNativeAsioWriter *bn_asio_create_writer(long channels, void *opaque,
    BnAsioCallback callback, BnAsioReset reset, unsigned int frames) {
    BnNativeAsioWriter *writer = new (std::nothrow) BnNativeAsioWriter{};
    if (!writer)
        return nullptr;
    writer->infos = new (std::nothrow) ASIOBufferInfo[channels]{};
    writer->current = new (std::nothrow) unsigned char *[channels]{};
    if (!writer->infos || !writer->current) {
        free_writer(writer);
        return nullptr;
    }
    writer->channels = channels;
    writer->frames = static_cast<long>(frames);
    writer->opaque = opaque;
    writer->callback = callback;
    writer->reset = reset;
    for (long channel = 0; channel < channels; ++channel) {
        writer->infos[channel].isInput = ASIOFalse;
        writer->infos[channel].channelNum = channel;
    }
    if (ASIOCreateBuffers(writer->infos, channels, writer->frames, &callbacks) != ASE_OK) {
        free_writer(writer);
        return nullptr;
    }
    return writer;
}

extern "C" int bn_asio_start_writer(BnNativeAsioWriter *writer) {
    // Both buffers contain format-appropriate silence before the driver can expose either.
    render_buffer(writer, 0);
    render_buffer(writer, 1);
    active_writer.store(writer, std::memory_order_release);
    if (ASIOStart() != ASE_OK) {
        active_writer.store(nullptr, std::memory_order_release);
        return -1;
    }
    writer->started = true;
    return 0;
}

extern "C" int bn_asio_stop_writer(BnNativeAsioWriter *writer) {
    if (writer->started) {
        if (ASIOStop() != ASE_OK)
            return -1;
        writer->started = false;
        active_writer.store(nullptr, std::memory_order_release);
    }
    if (ASIODisposeBuffers() != ASE_OK)
        return -1;
    free_writer(writer);
    return 0;
}
