/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#ifndef GUACENC_VIDEO_STREAM_H
#define GUACENC_VIDEO_STREAM_H

#include "buffer.h"

#include <stddef.h>

/**
 * The mimetype of guac-extra video streams (see guac-extra docs/PROTOCOL.md).
 */
#define GUACENC_VIDEO_STREAM_MIMETYPE "video/x-guac-extra-h264"

/**
 * A guac-extra video stream within a recording: H.264 parts, each behind a
 * 12-byte header, decoded with libavcodec onto a layer.
 */
typedef struct guacenc_video_stream guacenc_video_stream;

/**
 * Creates a video stream drawing onto the layer with the given index.
 * Returns NULL if no H.264 decoder is available.
 */
guacenc_video_stream* guacenc_video_stream_alloc(int layer_index);

/**
 * Returns the index of the layer the stream draws onto.
 */
int guacenc_video_stream_layer(guacenc_video_stream* stream);

/**
 * Adds received bytes to the stream, decoding every part they complete and
 * drawing each decoded picture at (0, 0) of `buffer`. Returns zero on
 * success.
 */
int guacenc_video_stream_receive(guacenc_video_stream* stream,
        const unsigned char* data, size_t length, guacenc_buffer* buffer);

/**
 * Frees the stream. NULL is ignored.
 */
void guacenc_video_stream_free(guacenc_video_stream* stream);

#endif
