/* TAK's official decoder supplies source samples; the audio decoder owns resampling and time. */
#include "player_core_internal.h"

/* The SDK requires packed structures, including on x64. The vendor header is kept unchanged. */
#pragma pack(push, 1)
#include "../../third_party/tak/tak_deco_lib.h"
#pragma pack(pop)

struct BnTakDecoder {
    HMODULE library;
    TtakSeekableStreamDecoder handle;
    void (*destroy)(TtakSeekableStreamDecoder);
    TtakResult (*state)(TtakSeekableStreamDecoder);
    TtakResult (*error_string)(TtakResult, TtakAnsiChar *, TtakInt32);
    TtakResult (*read)(TtakSeekableStreamDecoder, void *, TtakInt32, TtakInt32 *);
    TtakResult (*seek)(TtakSeekableStreamDecoder, TtakInt64);
    TtakInt64 (*position)(TtakSeekableStreamDecoder);
    BnByteSource source;
    BnInterrupt *interrupt;
    int32_t *transport_error;
    uint8_t *packed;
    int32_t *samples;
    int frames;
    int channels;
    int bytes_per_sample;
};

void bn_tak_destroy(BnTakDecoder *tak) {
    if (!tak)
        return;
    if (tak->handle)
        tak->destroy(tak->handle);
    free(tak->packed);
    free(tak->samples);
    if (tak->library)
        FreeLibrary(tak->library);
    free(tak);
}

static int bn_tak_error(BnTakDecoder *tak, TtakResult result, const char *operation,
                         char *error, size_t capacity) {
    char detail[tak_ErrorStringSizeMax] = {0};
    tak->error_string(result, detail, sizeof(detail));
    bn_error(error, capacity, "TAK %s failed (%d): %s", operation, result, detail);
    return AVERROR_INVALIDDATA;
}

static TtakBool bn_tak_can_read(void *user) { (void) user; return tak_True; }
static TtakBool bn_tak_cannot_write(void *user) { (void) user; return tak_False; }

static TtakBool bn_tak_read_bytes(void *user, void *buffer, TtakInt32 size,
                                 TtakInt32 *read_size) {
    BnTakDecoder *tak = user;
    *read_size = 0;
    while (*read_size < size) {
        int read = bn_source_read(&tak->source, tak->interrupt,
                                   (uint8_t *) buffer + *read_size, size - *read_size);
        if (read == AVERROR_EOF || read == 0)
            return tak_True;
        if (read < 0) {
            *tak->transport_error = read;
            return tak_False;
        }
        *read_size += read;
    }
    return tak_True;
}

static TtakBool bn_tak_seek_bytes(void *user, TtakInt64 position) {
    BnTakDecoder *tak = user;
    const int64_t result = tak->source.seek(tak->source.opaque, position, SEEK_SET);
    if (result < 0) *tak->transport_error = (int32_t) result;
    return result == position;
}

static TtakBool bn_tak_byte_length(void *user, TtakInt64 *length) {
    BnTakDecoder *tak = user;
    *length = tak->source.seek(tak->source.opaque, 0, AVSEEK_SIZE);
    return *length >= 0;
}

/* Only this executable's directory is searched. A current directory or PATH entry is not the
 * application's copy, and loading one would make a missing deployment appear to work. */
static HMODULE bn_tak_load(char *error, size_t capacity) {
    wchar_t path[32768];
    DWORD size = GetModuleFileNameW(NULL, path, sizeof(path) / sizeof(path[0]));
    if (!size || size >= sizeof(path) / sizeof(path[0])) {
        bn_error(error, capacity, "could not locate tak_deco_lib.dll beside the executable");
        return NULL;
    }
    wchar_t *name = wcsrchr(path, L'\\');
    if (!name || (size_t) (name + 1 - path) + sizeof(L"tak_deco_lib.dll") / sizeof(wchar_t)
                    > sizeof(path) / sizeof(path[0])) {
        bn_error(error, capacity, "executable path has no room for tak_deco_lib.dll");
        return NULL;
    }
    memcpy(name + 1, L"tak_deco_lib.dll", sizeof(L"tak_deco_lib.dll"));
    HMODULE library = LoadLibraryW(path);
    if (!library)
        bn_error(error, capacity, "could not load tak_deco_lib.dll beside the executable (Windows error %lu)",
                 GetLastError());
    return library;
}

int bn_tak_open(BnAudioDecoder *decoder, const wchar_t *path, const BnByteSource *source,
                AVChannelLayout *layout, char *error, size_t capacity) {
    BnTakDecoder *tak = calloc(1, sizeof(*tak));
    if (!tak) {
        bn_error(error, capacity, "could not allocate the TAK decoder");
        return AVERROR(ENOMEM);
    }
    decoder->tak = tak;
    tak->interrupt = &decoder->interrupt;
    tak->transport_error = &decoder->transport_error;
    tak->library = bn_tak_load(error, capacity);
    if (!tak->library)
        return AVERROR_EXTERNAL;

    TtakResult (*version)(TtakInt32 *, TtakInt32 *);
    TtakSeekableStreamDecoder (*open_file)(const TtakWideChar *, const TtakSSDOptions *,
                                           TSSDDamageCallback, void *);
    TtakSeekableStreamDecoder (*open_stream)(const TtakStreamIoInterface *, void *,
                                             const TtakSSDOptions *, TSSDDamageCallback, void *);
    TtakResult (*info)(TtakSeekableStreamDecoder, Ttak_str_StreamInfo_V22 *);
    TtakBool (*valid)(TtakSeekableStreamDecoder);

    /* Copying the address preserves the SDK's cdecl signatures without a function-pointer cast. */
#define BN_TAK_IMPORT(target, name) do { \
        FARPROC address = GetProcAddress(tak->library, name); \
        if (!address) { \
            bn_error(error, capacity, "tak_deco_lib.dll is missing %s", name); \
            return AVERROR_EXTERNAL; \
        } \
        memcpy(&(target), &address, sizeof(target)); \
    } while (0)
    BN_TAK_IMPORT(version, "tak_GetLibraryVersion");
    BN_TAK_IMPORT(open_file, "tak_SSD_Create_FromFileW");
    BN_TAK_IMPORT(open_stream, "tak_SSD_Create_FromStream");
    BN_TAK_IMPORT(info, "tak_SSD_GetStreamInfo_V22");
    BN_TAK_IMPORT(valid, "tak_SSD_Valid");
    BN_TAK_IMPORT(tak->destroy, "tak_SSD_Destroy");
    BN_TAK_IMPORT(tak->state, "tak_SSD_State");
    BN_TAK_IMPORT(tak->error_string, "tak_SSD_GetErrorString");
    BN_TAK_IMPORT(tak->read, "tak_SSD_ReadAudio");
    BN_TAK_IMPORT(tak->seek, "tak_SSD_Seek");
    BN_TAK_IMPORT(tak->position, "tak_SSD_GetReadPos");
#undef BN_TAK_IMPORT

    TtakInt32 installed = 0, compatibility = 0;
    if (version(&installed, &compatibility) != tak_res_Ok ||
        installed < tak_InterfaceVersion || compatibility > tak_InterfaceVersion) {
        bn_error(error, capacity, "tak_deco_lib.dll has an incompatible interface");
        return AVERROR_EXTERNAL;
    }
    const TtakSSDOptions options = {tak_Cpu_Any, 0};
    if (path) {
        tak->handle = open_file(path, &options, NULL, NULL);
    } else {
        tak->source = *source;
        const TtakStreamIoInterface io = {
            bn_tak_can_read, bn_tak_cannot_write, bn_tak_can_read,
            bn_tak_read_bytes, NULL, NULL, NULL, bn_tak_seek_bytes, bn_tak_byte_length,
        };
        if (!bn_tak_seek_bytes(tak, 0)) {
            bn_error(error, capacity, "could not rewind the TAK byte source");
            return AVERROR(EIO);
        }
        tak->handle = open_stream(&io, tak, &options, NULL, NULL);
    }
    if (!tak->handle) {
        bn_error(error, capacity, "tak_deco_lib.dll could not allocate a stream decoder");
        return AVERROR(ENOMEM);
    }
    if (!valid(tak->handle) || tak->state(tak->handle) >= tak_res_ssd_ErrorFirst)
        return bn_tak_error(tak, tak->state(tak->handle), "open", error, capacity);
    Ttak_str_StreamInfo_V22 stream = {0};
    TtakResult result = info(tak->handle, &stream);
    if (result != tak_res_Ok)
        return bn_tak_error(tak, result, "stream information", error, capacity);

    /* This is an external library's description of the allocation it will write into. */
    const TtakAudioFormatEx *audio = &stream.Audio;
    if (audio->DataType != tak_AudioFormat_DataType_PCM || audio->SampleRate <= 0 ||
        audio->ChannelNum <= 0 || audio->ChannelNum > tak_str_ChannelNumMax ||
        audio->SampleBits < 8 || audio->SampleBits > 24 ||
        audio->BlockSize != audio->ChannelNum * ((audio->SampleBits + 7) / 8) ||
        stream.Sizes.FrameSizeInSamples <= 0 || stream.Sizes.FrameSizeInSamples > tak_FrameSizeMax) {
        bn_error(error, capacity, "TAK decoder returned an unsupported PCM format");
        return AVERROR_INVALIDDATA;
    }
    tak->frames = stream.Sizes.FrameSizeInSamples;
    tak->channels = audio->ChannelNum;
    tak->bytes_per_sample = (audio->SampleBits + 7) / 8;
    tak->packed = malloc((size_t) tak->frames * audio->BlockSize);
    tak->samples = malloc((size_t) tak->frames * tak->channels * sizeof(int32_t));
    if (!tak->packed || !tak->samples) {
        bn_error(error, capacity, "could not allocate TAK PCM storage");
        return AVERROR(ENOMEM);
    }
    if (audio->HasExtension && audio->HasSpeakerAssignment) {
        result = av_channel_layout_custom_init(layout, audio->ChannelNum);
        if (result < 0)
            return result;
        for (int i = 0; i < audio->ChannelNum; ++i) {
            int speaker = (unsigned char) audio->SpeakerAssignment[i];
            layout->u.map[i].id = speaker >= 1 && speaker <= tak_Speaker_Max_Supported
                                      ? (enum AVChannel) (speaker - 1) : AV_CHAN_UNKNOWN;
        }
    } else {
        av_channel_layout_default(layout, audio->ChannelNum);
    }
    decoder->input_sample_rate = audio->SampleRate;
    return 0;
}

int bn_tak_receive(BnAudioDecoder *decoder, char *error, size_t capacity) {
    BnTakDecoder *tak = decoder->tak;
    int64_t position = tak->position(tak->handle);
    TtakInt32 frames = 0;
    bn_interrupt_deadline(tak->interrupt, BN_READ_DEADLINE_US);
    TtakResult result = tak->read(tak->handle, tak->packed, tak->frames, &frames);
    const int interrupted = bn_interrupt_expired(tak->interrupt);
    bn_interrupt_deadline(tak->interrupt, 0);
    if (interrupted) {
        decoder->transport_error = AVERROR_EXIT;
        bn_error(error, capacity, "TAK read cancelled or timed out");
        return AVERROR_EXIT;
    }
    if (result != tak_res_Ok)
        return bn_tak_error(tak, result, "decode", error, capacity);
    if (!frames) {
        decoder->input_eof = 1;
        return AVERROR_EOF;
    }
    /* TAK returns the original little-endian WAV samples. Expand packed 8/16/24-bit PCM to
     * left-aligned signed 32-bit samples, which the shared resampler accepts directly. */
    for (int i = 0; i < frames * tak->channels; ++i) {
        const uint8_t *sample = tak->packed + (size_t) i * tak->bytes_per_sample;
        uint32_t value = 0;
        for (int byte = 0; byte < tak->bytes_per_sample; ++byte)
            value |= (uint32_t) sample[byte] << (8 * byte);
        if (tak->bytes_per_sample == 1)
            value ^= 0x80;
        value <<= 32 - 8 * tak->bytes_per_sample;
        memcpy(&tak->samples[i], &value, sizeof(value));
    }
    decoder->frame->data[0] = (uint8_t *) tak->samples;
    decoder->frame->nb_samples = frames;
    decoder->frame->pts = av_rescale_q(position, (AVRational) {1, decoder->input_sample_rate},
                                      decoder->format->streams[decoder->audio_index]->time_base);
    decoder->frame->best_effort_timestamp = decoder->frame->pts;
    return 0;
}

int bn_tak_seek(BnAudioDecoder *decoder, int64_t pts, char *error, size_t capacity) {
    int64_t sample = av_rescale_q(pts, decoder->format->streams[decoder->audio_index]->time_base,
                                  (AVRational) {1, decoder->input_sample_rate});
    bn_interrupt_deadline(&decoder->interrupt, BN_READ_DEADLINE_US);
    TtakResult result = decoder->tak->seek(decoder->tak->handle, sample);
    const int interrupted = bn_interrupt_expired(&decoder->interrupt);
    bn_interrupt_deadline(&decoder->interrupt, 0);
    if (interrupted) {
        decoder->transport_error = AVERROR_EXIT;
        bn_error(error, capacity, "TAK seek cancelled or timed out");
        return AVERROR_EXIT;
    }
    return result == tak_res_Ok ? 0
        : bn_tak_error(decoder->tak, result, "seek", error, capacity);
}
