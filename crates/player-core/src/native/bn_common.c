/* Error text, the open deadline's interrupt callback, and the custom-AVIO context every
 * byte-source open goes through. */
#include "player_core_internal.h"

/* Only transport statuses are admitted. HTTP 4xx, invalid data and generic external failures
 * deliberately do not qualify. EXIT is a cooperative deadline; hosts suppress cancelled work. */
int32_t bn_error_is_network_failure(int32_t code) {
    return code == AVERROR_EXIT || code == AVERROR_HTTP_SERVER_ERROR ||
        code == AVERROR(ETIMEDOUT) ||
        code == AVERROR(ECONNRESET) || code == AVERROR(ECONNABORTED) ||
        code == AVERROR(ECONNREFUSED) || code == AVERROR(ENETDOWN) ||
        code == AVERROR(ENETRESET) || code == AVERROR(ENETUNREACH) ||
        code == AVERROR(EHOSTUNREACH) || code == AVERROR(ENOTCONN) ||
        code == AVERROR(EPIPE) ||
        /* Windows FFmpeg socket failures can carry Winsock, not CRT errno values. */
        code == AVERROR(WSAETIMEDOUT) || code == AVERROR(WSAECONNRESET) ||
        code == AVERROR(WSAECONNABORTED) || code == AVERROR(WSAECONNREFUSED) ||
        code == AVERROR(WSAENETDOWN) || code == AVERROR(WSAENETRESET) ||
        code == AVERROR(WSAENETUNREACH) || code == AVERROR(WSAEHOSTUNREACH) ||
        code == AVERROR(WSAENOTCONN);
}

/* Stable private bridge from Rust ErrorKind. Unknown I/O must not turn into retryable EIO. */
int32_t bn_io_error_code(int32_t kind) {
    switch (kind) {
    case 1: return AVERROR(ETIMEDOUT);
    case 2: return AVERROR(ECONNRESET);
    case 3: return AVERROR(ECONNABORTED);
    case 4: return AVERROR(ECONNREFUSED);
    case 5: return AVERROR(ENOTCONN);
    case 6: return AVERROR(EPIPE);
    case 7: return AVERROR(ENETUNREACH);
    case 8: return AVERROR(EHOSTUNREACH);
    case 9: return AVERROR(EACCES);
    case 10: return AVERROR(EINVAL);
    case 11: return AVERROR_INVALIDDATA;
    case 12: return AVERROR(EINTR);
    default: return AVERROR_EXTERNAL;
    }
}

int bn_interrupt_expired(void *opaque) {
    BnInterrupt *interrupt = opaque;
    if (!interrupt)
        return 0;
    if (InterlockedCompareExchange(&interrupt->cancelled, 0, 0) ||
        (interrupt->external_cancelled &&
         interrupt->external_cancelled(interrupt->cancel_opaque)))
        return 1;
    const int64_t deadline = InterlockedCompareExchange64(&interrupt->deadline_us, 0, 0);
    return deadline != 0 && av_gettime_relative() >= deadline;
}

void bn_interrupt_deadline(BnInterrupt *interrupt, int64_t budget_us) {
    InterlockedExchange64(&interrupt->deadline_us,
                          budget_us > 0 ? av_gettime_relative() + budget_us : 0);
}

/* A custom-AVIO loop can read many chunks without polling AVFormatContext's interrupt.
 * Carry the same operation budget into every transport read, not a fresh timeout per chunk. */
int bn_source_read(const BnByteSource *source, BnInterrupt *interrupt,
                   uint8_t *buffer, int size) {
    if (bn_interrupt_expired(interrupt))
        return AVERROR_EXIT;
    const int64_t deadline = InterlockedCompareExchange64(&interrupt->deadline_us, 0, 0);
    const int64_t remaining = deadline ? deadline - av_gettime_relative() : 0;
    if (deadline && remaining <= 0)
        return AVERROR_EXIT;
    const int result = deadline && source->read_timed
        ? source->read_timed(source->opaque, buffer, size, remaining)
        : source->read(source->opaque, buffer, size);
    return bn_interrupt_expired(interrupt) ? AVERROR_EXIT : result;
}

typedef struct BnCustomSource {
    BnByteSource source;
    BnInterrupt *interrupt;
} BnCustomSource;

static int bn_custom_read(void *opaque, uint8_t *buffer, int size) {
    BnCustomSource *input = opaque;
    return bn_source_read(&input->source, input->interrupt, buffer, size);
}

static int64_t bn_custom_seek(void *opaque, int64_t offset, int whence) {
    BnCustomSource *input = opaque;
    if (bn_interrupt_expired(input->interrupt))
        return AVERROR_EXIT;
    const int64_t result = input->source.seek(input->source.opaque, offset, whence);
    return bn_interrupt_expired(input->interrupt) ? AVERROR_EXIT : result;
}

void bn_error(char *error, size_t capacity, const char *format, ...) {
    if (!error || capacity == 0)
        return;
    va_list arguments;
    va_start(arguments, format);
    vsnprintf(error, capacity, format, arguments);
    va_end(arguments);
    error[capacity - 1] = '\0';
}

void bn_av_error(char *error, size_t capacity, const char *operation,
                 int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE] = {0};
    av_strerror(code, detail, sizeof(detail));
    bn_error(error, capacity, "%s: %s (%d)", operation, detail, code);
}

void bn_custom_io_free(AVIOContext **custom_io) {
    if (!custom_io || !*custom_io)
        return;
    av_freep(&(*custom_io)->opaque);
    av_freep(&(*custom_io)->buffer);
    avio_context_free(custom_io);
}

void bn_copy_text(char *destination, size_t capacity,
                  const char *source) {
    if (!destination || capacity == 0)
        return;
    snprintf(destination, capacity, "%s", source ? source : "");
    destination[capacity - 1] = '\0';
}

int bn_custom_input_open(AVFormatContext **format,
                         AVIOContext **custom_io,
                         const BnByteSource *source, BnInterrupt *interrupt) {
    *format = avformat_alloc_context();
    *custom_io = NULL;
    if (!*format)
        return AVERROR(ENOMEM);

    uint8_t *buffer = (uint8_t *) av_malloc(BN_AVIO_BUFFER_SIZE);
    if (!buffer) {
        avformat_free_context(*format);
        *format = NULL;
        return AVERROR(ENOMEM);
    }

    BnCustomSource *input = av_mallocz(sizeof(*input));
    if (!input) {
        av_free(buffer);
        avformat_free_context(*format);
        *format = NULL;
        return AVERROR(ENOMEM);
    }
    input->source = *source;
    input->interrupt = interrupt;
    AVIOContext *io = avio_alloc_context(
        buffer, BN_AVIO_BUFFER_SIZE, 0, input, bn_custom_read, NULL, bn_custom_seek);
    if (!io) {
        av_free(input);
        av_free(buffer);
        avformat_free_context(*format);
        *format = NULL;
        return AVERROR(ENOMEM);
    }

    *custom_io = io;
    (*format)->pb = io;
    interrupt->external_cancelled = source->cancelled;
    interrupt->cancel_opaque = source->opaque;
    bn_interrupt_deadline(interrupt, BN_OPEN_DEADLINE_US);
    (*format)->interrupt_callback = (AVIOInterruptCB){bn_interrupt_expired, interrupt};
    if (bn_interrupt_expired(interrupt))
        return AVERROR_EXIT;
    return avformat_open_input(format, NULL, NULL, NULL);
}
