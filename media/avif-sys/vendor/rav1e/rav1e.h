// SPDX-License-Identifier: MIT
#ifndef RAV1E_H
#define RAV1E_H

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

typedef enum {
    RA_CHROMA_SAMPLING_CS420,
    RA_CHROMA_SAMPLING_CS422,
    RA_CHROMA_SAMPLING_CS444,
    RA_CHROMA_SAMPLING_CS400,
} RaChromaSampling;

typedef enum {
    RA_CHROMA_SAMPLE_POSITION_UNKNOWN,
    RA_CHROMA_SAMPLE_POSITION_VERTICAL,
    RA_CHROMA_SAMPLE_POSITION_COLOCATED,
} RaChromaSamplePosition;

typedef enum {
    RA_PIXEL_RANGE_LIMITED,
    RA_PIXEL_RANGE_FULL,
} RaPixelRange;

typedef int RaMatrixCoefficients;
typedef int RaColorPrimaries;
typedef int RaTransferCharacteristics;

typedef enum {
    RA_FRAME_TYPE_OVERRIDE_NO,
    RA_FRAME_TYPE_OVERRIDE_KEY,
} RaFrameTypeOverride;

typedef enum {
    RA_ENCODER_STATUS_SUCCESS,
    RA_ENCODER_STATUS_NEED_MORE_DATA,
    RA_ENCODER_STATUS_ENOUGH_DATA,
    RA_ENCODER_STATUS_LIMIT_REACHED,
    RA_ENCODER_STATUS_ENCODED,
    RA_ENCODER_STATUS_FAILURE = -1,
    RA_ENCODER_STATUS_NOT_READY = -2,
} RaEncoderStatus;

typedef enum {
    RA_FRAME_TYPE_KEY,
    RA_FRAME_TYPE_INTER,
    RA_FRAME_TYPE_INTRA_ONLY,
    RA_FRAME_TYPE_SWITCH,
} RaFrameType;

typedef struct RaConfig RaConfig;
typedef struct RaContext RaContext;
typedef struct RaFrame RaFrame;

typedef struct {
    const uint8_t *data;
    size_t len;
    uint64_t input_frameno;
    RaFrameType frame_type;
    void *opaque;
    RaFrame *rec;
    RaFrame *source;
} RaPacket;

#ifdef __cplusplus
extern "C" {
#endif

const char *rav1e_version_short(void);
const char *rav1e_version_full(void);
RaConfig *rav1e_config_default(void);
int rav1e_config_set_pixel_format(RaConfig *cfg, uint8_t bit_depth,
                                  RaChromaSampling subsampling,
                                  RaChromaSamplePosition chroma_pos,
                                  RaPixelRange pixel_range);
int rav1e_config_set_color_description(RaConfig *cfg, RaMatrixCoefficients matrix,
                                       RaColorPrimaries primaries,
                                       RaTransferCharacteristics transfer);
void rav1e_config_unref(RaConfig *cfg);
int rav1e_config_parse(RaConfig *cfg, const char *key, const char *value);
int rav1e_config_parse_int(RaConfig *cfg, const char *key, int value);
RaContext *rav1e_context_new(const RaConfig *cfg);
void rav1e_context_unref(RaContext *ctx);
RaFrame *rav1e_frame_new(const RaContext *ctx);
void rav1e_frame_unref(RaFrame *frame);
int rav1e_frame_set_type(RaFrame *frame, RaFrameTypeOverride frame_type);
void rav1e_frame_fill_plane(RaFrame *frame, int plane, const uint8_t *data,
                            size_t data_len, ptrdiff_t stride, int bytewidth);
RaEncoderStatus rav1e_send_frame(RaContext *ctx, RaFrame *frame);
RaEncoderStatus rav1e_receive_packet(RaContext *ctx, RaPacket **pkt);
void rav1e_packet_unref(RaPacket *pkt);

#ifdef __cplusplus
}
#endif
#endif
