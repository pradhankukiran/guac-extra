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
#include "display.h"
#include "log.h"
#include "video-stream.h"

#include <guacamole/client.h>

#include <stdlib.h>
#include <string.h>

int guacenc_handle_video(guacenc_display* display, int argc, char** argv) {

    /* Verify argument count */
    if (argc < 3) {
        guacenc_log(GUAC_LOG_WARNING, "\"video\" instruction incomplete");
        return 1;
    }

    int index = atoi(argv[0]);
    int layer_index = atoi(argv[1]);
    const char* mimetype = argv[2];

    if (strcmp(mimetype, GUACENC_VIDEO_STREAM_MIMETYPE) != 0) {
        guacenc_log(GUAC_LOG_DEBUG, "Ignoring video stream of type \"%s\".", mimetype);
        return 0;
    }

    return guacenc_display_create_video_stream(display, index, layer_index);

}
