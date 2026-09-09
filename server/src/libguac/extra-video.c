/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#include "guacamole/extra-video.h"

#include <stddef.h>

#ifdef ENABLE_GUAC_EXTRA_VIDEO
#include <guac_extra_video.h>
#endif

const char* guac_extra_video_version(void) {
#ifdef ENABLE_GUAC_EXTRA_VIDEO
    return gx_version();
#else
    return NULL;
#endif
}
