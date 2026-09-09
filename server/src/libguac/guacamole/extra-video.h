/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#ifndef GUAC_EXTRA_VIDEO_PUBLIC_H
#define GUAC_EXTRA_VIDEO_PUBLIC_H

/**
 * Access to the guac-extra video library (video/ in the guac-extra tree).
 * libguac links that library when configured with --with-guac-extra-video.
 *
 * @file extra-video.h
 */

/**
 * Returns the version of the linked guac-extra video library.
 *
 * @return
 *     The library version as a static string that must not be freed, or
 *     NULL if libguac was built without the video library.
 */
const char* guac_extra_video_version(void);

#endif
