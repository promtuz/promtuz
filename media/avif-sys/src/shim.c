#include <avif/avif.h>
#include <limits.h>
#include <stdint.h>
#include <string.h>

typedef struct {
    uint8_t *pixels;
    size_t pixel_bytes;
    uint32_t width;
    uint32_t height;
    uint32_t source_depth;
    uint32_t frame_count;
    uint32_t duration_ms;
    uint16_t color_primaries;
    uint16_t transfer_characteristics;
    uint16_t matrix_coefficients;
    uint8_t full_range;
    uint8_t has_alpha;
    uint8_t has_gain_map;
} PromtuzAvifImage;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t source_depth;
    uint32_t frame_count;
    uint16_t color_primaries;
    uint16_t transfer_characteristics;
    uint16_t matrix_coefficients;
    uint8_t full_range;
    uint8_t has_alpha;
    uint8_t has_gain_map;
} PromtuzAvifInfo;

typedef struct {
    avifDecoder *decoder;
} PromtuzAvifDecoder;

typedef struct {
    avifEncoder *encoder;
    uint32_t width;
    uint32_t height;
    avifBool animated;
    avifBool wrote_frame;
} PromtuzAvifEncoder;

enum { PROMTUZ_AVIF_LIMIT_EXCEEDED = -1 };

static void promtuz_avif_info(const avifDecoder *decoder, PromtuzAvifInfo *output) {
    output->width = decoder->image->width;
    output->height = decoder->image->height;
    output->source_depth = decoder->image->depth;
    output->frame_count = (uint32_t)decoder->imageCount;
    output->color_primaries = decoder->image->colorPrimaries;
    output->transfer_characteristics = decoder->image->transferCharacteristics;
    output->matrix_coefficients = decoder->image->matrixCoefficients;
    output->full_range = decoder->image->yuvRange == AVIF_RANGE_FULL;
    output->has_alpha = decoder->alphaPresent;
    output->has_gain_map = decoder->image->gainMap != NULL;
}

int promtuz_avif_decoder_open(const uint8_t *input, size_t input_len,
                              uint32_t max_pixels, uint32_t max_edge,
                              uint32_t max_frames, PromtuzAvifInfo *info,
                              PromtuzAvifDecoder **output) {
    if (input == NULL || input_len == 0 || max_pixels == 0 || max_edge == 0 ||
        max_frames == 0 || info == NULL || output == NULL) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    memset(info, 0, sizeof(*info));
    *output = NULL;

    avifDecoder *decoder = avifDecoderCreate();
    if (decoder == NULL) {
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    decoder->maxThreads = 1;
    decoder->ignoreExif = AVIF_TRUE;
    decoder->ignoreXMP = AVIF_TRUE;
    decoder->imageCountLimit = max_frames;

    avifResult result = avifDecoderSetIOMemory(decoder, input, input_len);
    if (result == AVIF_RESULT_OK) {
        result = avifDecoderParse(decoder);
    }
    if (result != AVIF_RESULT_OK) {
        avifDecoderDestroy(decoder);
        return result;
    }
    if (decoder->imageCount <= 0 || (uint32_t)decoder->imageCount > max_frames) {
        avifDecoderDestroy(decoder);
        return PROMTUZ_AVIF_LIMIT_EXCEEDED;
    }
    if (decoder->image->width > max_edge || decoder->image->height > max_edge ||
        (uint64_t)decoder->image->width * decoder->image->height > max_pixels) {
        avifDecoderDestroy(decoder);
        return PROMTUZ_AVIF_LIMIT_EXCEEDED;
    }
    decoder->imageSizeLimit = max_pixels;
    decoder->imageDimensionLimit = max_edge;
    PromtuzAvifDecoder *state = avifAlloc(sizeof(*state));
    if (state == NULL) {
        avifDecoderDestroy(decoder);
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    state->decoder = decoder;
    promtuz_avif_info(decoder, info);
    *output = state;
    return AVIF_RESULT_OK;
}

int promtuz_avif_decoder_open_file(const char *path,
                                   uint32_t max_pixels, uint32_t max_edge,
                                   uint32_t max_frames, PromtuzAvifInfo *info,
                                   PromtuzAvifDecoder **output) {
    if (path == NULL || path[0] == '\0' || max_pixels == 0 || max_edge == 0 ||
        max_frames == 0 || info == NULL || output == NULL) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    memset(info, 0, sizeof(*info));
    *output = NULL;

    avifDecoder *decoder = avifDecoderCreate();
    if (decoder == NULL) {
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    decoder->maxThreads = 1;
    decoder->ignoreExif = AVIF_TRUE;
    decoder->ignoreXMP = AVIF_TRUE;
    decoder->imageCountLimit = max_frames;

    avifResult result = avifDecoderSetIOFile(decoder, path);
    if (result == AVIF_RESULT_OK) {
        result = avifDecoderParse(decoder);
    }
    if (result != AVIF_RESULT_OK) {
        avifDecoderDestroy(decoder);
        return result;
    }
    if (decoder->imageCount <= 0 || (uint32_t)decoder->imageCount > max_frames ||
        decoder->image->width > max_edge || decoder->image->height > max_edge ||
        (uint64_t)decoder->image->width * decoder->image->height > max_pixels) {
        avifDecoderDestroy(decoder);
        return PROMTUZ_AVIF_LIMIT_EXCEEDED;
    }
    decoder->imageSizeLimit = max_pixels;
    decoder->imageDimensionLimit = max_edge;
    PromtuzAvifDecoder *state = avifAlloc(sizeof(*state));
    if (state == NULL) {
        avifDecoderDestroy(decoder);
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    state->decoder = decoder;
    promtuz_avif_info(decoder, info);
    *output = state;
    return AVIF_RESULT_OK;
}

int promtuz_avif_decoder_next(PromtuzAvifDecoder *state, PromtuzAvifImage *output) {
    if (state == NULL || state->decoder == NULL || output == NULL) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    memset(output, 0, sizeof(*output));
    avifDecoder *decoder = state->decoder;
    avifResult result = avifDecoderNextImage(decoder);
    if (result != AVIF_RESULT_OK) {
        return result;
    }

    avifRGBImage rgb;
    avifRGBImageSetDefaults(&rgb, decoder->image);
    rgb.format = AVIF_RGB_FORMAT_RGBA;
    rgb.depth = 16;
    rgb.maxThreads = 1;
    result = avifRGBImageAllocatePixels(&rgb);
    if (result == AVIF_RESULT_OK) {
        result = avifImageYUVToRGB(decoder->image, &rgb);
    }
    if (result != AVIF_RESULT_OK) {
        avifRGBImageFreePixels(&rgb);
        return result;
    }
    if (rgb.rowBytes == 0 || rgb.height > SIZE_MAX / rgb.rowBytes) {
        avifRGBImageFreePixels(&rgb);
        return PROMTUZ_AVIF_LIMIT_EXCEEDED;
    }

    output->pixels = rgb.pixels;
    output->pixel_bytes = (size_t)rgb.rowBytes * rgb.height;
    output->width = decoder->image->width;
    output->height = decoder->image->height;
    output->source_depth = decoder->image->depth;
    output->frame_count = (uint32_t)decoder->imageCount;
    uint64_t duration_ms = decoder->timescale == 0 ? 0 :
        (decoder->imageTiming.durationInTimescales * 1000 + decoder->timescale / 2) /
        decoder->timescale;
    output->duration_ms = duration_ms > UINT32_MAX ? UINT32_MAX : (uint32_t)duration_ms;
    output->color_primaries = decoder->image->colorPrimaries;
    output->transfer_characteristics = decoder->image->transferCharacteristics;
    output->matrix_coefficients = decoder->image->matrixCoefficients;
    output->full_range = decoder->image->yuvRange == AVIF_RANGE_FULL;
    output->has_alpha = decoder->alphaPresent;
    output->has_gain_map = decoder->image->gainMap != NULL;
    rgb.pixels = NULL;
    return AVIF_RESULT_OK;
}

void promtuz_avif_decoder_destroy(PromtuzAvifDecoder *state) {
    if (state != NULL) {
        avifDecoderDestroy(state->decoder);
        avifFree(state);
    }
}

int promtuz_avif_decode_rgba16(const uint8_t *input, size_t input_len,
                                uint32_t max_pixels, uint32_t max_edge,
                                uint32_t max_frames, PromtuzAvifImage *output) {
    PromtuzAvifInfo info;
    PromtuzAvifDecoder *decoder = NULL;
    int result = promtuz_avif_decoder_open(input, input_len, max_pixels, max_edge,
                                           max_frames, &info, &decoder);
    if (result == AVIF_RESULT_OK) {
        result = promtuz_avif_decoder_next(decoder, output);
    }
    promtuz_avif_decoder_destroy(decoder);
    return result;
}

void promtuz_avif_free_pixels(uint8_t *pixels) {
    avifFree(pixels);
}

int promtuz_avif_encoder_create(uint32_t width, uint32_t height, uint32_t quantizer,
                                int speed, uint32_t timescale, uint8_t animated,
                                PromtuzAvifEncoder **output) {
    if (width == 0 || height == 0 || quantizer > AVIF_QUANTIZER_WORST_QUALITY ||
        speed < AVIF_SPEED_SLOWEST || speed > AVIF_SPEED_FASTEST || timescale == 0 ||
        output == NULL) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    *output = NULL;
    PromtuzAvifEncoder *state = avifAlloc(sizeof(*state));
    if (state == NULL) {
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    memset(state, 0, sizeof(*state));
    state->encoder = avifEncoderCreate();
    if (state->encoder == NULL) {
        avifFree(state);
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    state->encoder->maxThreads = 2;
    state->encoder->speed = speed;
    state->encoder->quality =
        ((AVIF_QUANTIZER_WORST_QUALITY - (int)quantizer) * 100 + 31) /
        AVIF_QUANTIZER_WORST_QUALITY;
    state->encoder->qualityAlpha = state->encoder->quality;
    state->encoder->timescale = timescale;
    state->encoder->keyframeInterval = animated ? 120 : 0;
    state->encoder->repetitionCount = AVIF_REPETITION_COUNT_INFINITE;
    state->width = width;
    state->height = height;
    state->animated = animated ? AVIF_TRUE : AVIF_FALSE;
    *output = state;
    return AVIF_RESULT_OK;
}

int promtuz_avif_encoder_add_rgba(PromtuzAvifEncoder *state, const uint8_t *pixels,
                                  size_t pixel_bytes, uint32_t duration) {
    if (state == NULL || state->encoder == NULL || pixels == NULL || duration == 0 ||
        pixel_bytes != (size_t)state->width * state->height * 4) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    avifImage *image = avifImageCreate(state->width, state->height, 8, AVIF_PIXEL_FORMAT_YUV444);
    if (image == NULL) {
        return AVIF_RESULT_OUT_OF_MEMORY;
    }
    image->colorPrimaries = AVIF_COLOR_PRIMARIES_BT709;
    image->transferCharacteristics = AVIF_TRANSFER_CHARACTERISTICS_SRGB;
    image->matrixCoefficients = AVIF_MATRIX_COEFFICIENTS_BT601;
    image->yuvRange = AVIF_RANGE_FULL;

    avifRGBImage rgb;
    avifRGBImageSetDefaults(&rgb, image);
    rgb.format = AVIF_RGB_FORMAT_RGBA;
    rgb.depth = 8;
    rgb.pixels = (uint8_t *)pixels;
    rgb.rowBytes = state->width * 4;
    avifResult result = avifImageRGBToYUV(image, &rgb);
    if (result == AVIF_RESULT_OK) {
        const avifAddImageFlags flags = state->animated
            ? AVIF_ADD_IMAGE_FLAG_NONE
            : AVIF_ADD_IMAGE_FLAG_SINGLE;
        result = avifEncoderAddImage(state->encoder, image, duration, flags);
    }
    avifImageDestroy(image);
    if (result == AVIF_RESULT_OK) {
        state->wrote_frame = AVIF_TRUE;
    }
    return result;
}

int promtuz_avif_encoder_finish(PromtuzAvifEncoder *state, uint8_t **bytes, size_t *size) {
    if (state == NULL || state->encoder == NULL || !state->wrote_frame ||
        bytes == NULL || size == NULL) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    *bytes = NULL;
    *size = 0;
    avifRWData output = AVIF_DATA_EMPTY;
    avifResult result = avifEncoderFinish(state->encoder, &output);
    if (result == AVIF_RESULT_OK) {
        *bytes = output.data;
        *size = output.size;
    }
    return result;
}

void promtuz_avif_encoder_destroy(PromtuzAvifEncoder *state) {
    if (state != NULL) {
        avifEncoderDestroy(state->encoder);
        avifFree(state);
    }
}

void promtuz_avif_free_bytes(uint8_t *bytes) {
    avifFree(bytes);
}
