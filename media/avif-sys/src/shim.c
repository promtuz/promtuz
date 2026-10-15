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
    uint16_t color_primaries;
    uint16_t transfer_characteristics;
    uint16_t matrix_coefficients;
    uint8_t full_range;
    uint8_t has_alpha;
    uint8_t has_gain_map;
} PromtuzAvifImage;

enum { PROMTUZ_AVIF_LIMIT_EXCEEDED = -1 };

int promtuz_avif_decode_rgba16(const uint8_t *input, size_t input_len,
                                uint32_t max_pixels, uint32_t max_edge,
                                uint32_t max_frames, PromtuzAvifImage *output) {
    if (input == NULL || input_len == 0 || max_pixels == 0 || max_edge == 0 ||
        max_frames == 0 || output == NULL) {
        return AVIF_RESULT_INVALID_ARGUMENT;
    }
    memset(output, 0, sizeof(*output));

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
    result = avifDecoderNextImage(decoder);
    if (result != AVIF_RESULT_OK) {
        avifDecoderDestroy(decoder);
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
        avifDecoderDestroy(decoder);
        return result;
    }
    if (rgb.rowBytes == 0 || rgb.height > SIZE_MAX / rgb.rowBytes) {
        avifRGBImageFreePixels(&rgb);
        avifDecoderDestroy(decoder);
        return PROMTUZ_AVIF_LIMIT_EXCEEDED;
    }

    output->pixels = rgb.pixels;
    output->pixel_bytes = (size_t)rgb.rowBytes * rgb.height;
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
    rgb.pixels = NULL;
    avifDecoderDestroy(decoder);
    return AVIF_RESULT_OK;
}

void promtuz_avif_free_pixels(uint8_t *pixels) {
    avifFree(pixels);
}
