/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#include "config.h"
#include "buffer.h"
#include "log.h"
#include "video-stream.h"

#include <cairo/cairo.h>
#include <guacamole/client.h>
#include <guacamole/mem.h>
#include <libavcodec/avcodec.h>
#include <libavutil/frame.h>
#include <libswscale/swscale.h>

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/**
 * Size of the header before each part (see guac-extra docs/PROTOCOL.md).
 */
#define GUACENC_VIDEO_STREAM_HEADER 12

struct guacenc_video_stream {

    /**
     * The layer this stream draws onto.
     */
    int layer_index;

    /**
     * Received bytes not yet consumed as parts.
     */
    unsigned char* buf;
    size_t length;
    size_t capacity;

    AVCodecContext* codec;
    AVPacket* packet;
    AVFrame* frame;

    /**
     * Converter from the decoder's output to cairo's 32-bit layout, for
     * the current picture size.
     */
    struct SwsContext* sws;
    int sws_width;
    int sws_height;
    int sws_format;
    int sws_factor;

    /**
     * Whether a keyframe has been seen (parts before it cannot decode).
     */
    int started;

};

guacenc_video_stream* guacenc_video_stream_alloc(int layer_index) {

    const AVCodec* h264 = avcodec_find_decoder(AV_CODEC_ID_H264);
    if (h264 == NULL) {
        guacenc_log(GUAC_LOG_WARNING, "No H.264 decoder: video in this "
                "recording will not be rendered.");
        return NULL;
    }

    guacenc_video_stream* stream = guac_mem_zalloc(sizeof(guacenc_video_stream));
    stream->layer_index = layer_index;
    stream->codec = avcodec_alloc_context3(h264);
    stream->packet = av_packet_alloc();
    stream->frame = av_frame_alloc();

    if (stream->codec == NULL || stream->packet == NULL || stream->frame == NULL
            || avcodec_open2(stream->codec, h264, NULL) < 0) {
        guacenc_log(GUAC_LOG_WARNING, "Cannot open the H.264 decoder.");
        guacenc_video_stream_free(stream);
        return NULL;
    }

    return stream;

}

int guacenc_video_stream_layer(guacenc_video_stream* stream) {
    return stream->layer_index;
}

/**
 * Draws the decoder's current frame at (0, 0) of the buffer, clipped.
 */
static void guacenc_video_stream_draw(guacenc_video_stream* stream,
        guacenc_buffer* buffer) {

    AVFrame* frame = stream->frame;
    if (buffer == NULL || buffer->surface == NULL)
        return;

    if (frame->width <= 0 || frame->height <= 0)
        return;

    /* guacd halves the resolution on very slow links: scale such pictures
     * back up by the whole factor that fills the layer */
    int factor = (buffer->width + frame->width / 2) / frame->width;
    if (factor < 1)
        factor = 1;

    int width = frame->width * factor;
    int height = frame->height * factor;
    if (width > buffer->width) width = buffer->width;
    if (height > buffer->height) height = buffer->height;
    if (width <= 0 || height <= 0)
        return;

    /* Source rows/columns that map onto the (clipped) destination */
    int src_width = (width + factor - 1) / factor;
    int src_height = (height + factor - 1) / factor;

    if (stream->sws == NULL || stream->sws_width != width
            || stream->sws_height != height || stream->sws_format != frame->format
            || stream->sws_factor != factor) {
        sws_freeContext(stream->sws);
        stream->sws = sws_getContext(src_width, src_height, (enum AVPixelFormat) frame->format,
                width, height, AV_PIX_FMT_BGRA,
                factor > 1 ? SWS_BILINEAR : SWS_POINT, NULL, NULL, NULL);
        stream->sws_width = width;
        stream->sws_height = height;
        stream->sws_format = frame->format;
        stream->sws_factor = factor;
        if (stream->sws == NULL)
            return;
    }

    /* Cairo's 32-bit formats are BGRA in memory on little-endian machines */
    cairo_surface_flush(buffer->surface);
    uint8_t* dst[4] = { buffer->image, NULL, NULL, NULL };
    int dst_stride[4] = { buffer->stride, 0, 0, 0 };
    sws_scale(stream->sws, (const uint8_t* const*) frame->data,
            frame->linesize, 0, src_height, dst, dst_stride);

    /* Video pixels are opaque */
    for (int y = 0; y < height; y++) {
        uint8_t* row = buffer->image + y * buffer->stride;
        for (int x = 0; x < width; x++)
            row[x * 4 + 3] = 0xFF;
    }

    cairo_surface_mark_dirty(buffer->surface);

}

/**
 * Decodes one part's H.264 data and draws what comes out.
 */
static int guacenc_video_stream_decode(guacenc_video_stream* stream,
        const unsigned char* data, size_t length, guacenc_buffer* buffer) {

    stream->packet->data = (uint8_t*) data;
    stream->packet->size = (int) length;

    if (avcodec_send_packet(stream->codec, stream->packet) < 0) {
        guacenc_log(GUAC_LOG_DEBUG, "H.264 decoder rejected a part.");
        return 0;
    }

    while (avcodec_receive_frame(stream->codec, stream->frame) == 0)
        guacenc_video_stream_draw(stream, buffer);

    return 0;

}

int guacenc_video_stream_receive(guacenc_video_stream* stream,
        const unsigned char* data, size_t length, guacenc_buffer* buffer) {

    /* Append */
    if (stream->length + length > stream->capacity) {
        size_t capacity = stream->capacity ? stream->capacity : 65536;
        while (capacity < stream->length + length)
            capacity *= 2;
        stream->buf = guac_mem_realloc(stream->buf, capacity);
        stream->capacity = capacity;
    }
    memcpy(stream->buf + stream->length, data, length);
    stream->length += length;

    /* Consume every complete part */
    size_t offset = 0;
    while (stream->length - offset >= GUACENC_VIDEO_STREAM_HEADER) {

        const unsigned char* h = stream->buf + offset;
        if (h[0] != 1) {
            guacenc_log(GUAC_LOG_WARNING, "Unknown video format version %i.", h[0]);
            stream->length = 0;
            return 1;
        }

        size_t part = ((size_t) h[8] << 24) | ((size_t) h[9] << 16)
                    | ((size_t) h[10] << 8) | (size_t) h[11];
        if (stream->length - offset < GUACENC_VIDEO_STREAM_HEADER + part)
            break;

        int keyframe = h[1] & 0x01;
        stream->started |= keyframe;

        /* Parts forwarded from the remote desktop start with a region list
         * (count, then 8 bytes per region); skip it. The whole picture is
         * drawn: such parts are not recorded (passthrough is off while
         * recording), this only keeps guacenc from misreading them. */
        const unsigned char* h264 = h + GUACENC_VIDEO_STREAM_HEADER;
        size_t h264_length = part;
        if ((h[1] & 0x04) && part >= 2) {
            size_t skip = 2 + 8 * (((size_t) h264[0] << 8) | h264[1]);
            if (skip > part)
                skip = part;
            h264 += skip;
            h264_length -= skip;
        }

        if (stream->started)
            guacenc_video_stream_decode(stream, h264, h264_length, buffer);

        offset += GUACENC_VIDEO_STREAM_HEADER + part;

    }

    memmove(stream->buf, stream->buf + offset, stream->length - offset);
    stream->length -= offset;
    return 0;

}

void guacenc_video_stream_free(guacenc_video_stream* stream) {

    if (stream == NULL)
        return;

    sws_freeContext(stream->sws);
    av_frame_free(&stream->frame);
    av_packet_free(&stream->packet);
    avcodec_free_context(&stream->codec);
    guac_mem_free(stream->buf);
    guac_mem_free(stream);

}
