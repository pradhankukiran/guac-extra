/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#ifndef GUAC_DISPLAY_VIDEO_PRIV_H
#define GUAC_DISPLAY_VIDEO_PRIV_H

#include "display-plan.h"
#include "guacamole/display-types.h"
#include "guacamole/socket-types.h"
#include "guacamole/user-types.h"

/**
 * Internal hooks between guac_display and its video state. All are no-ops
 * when video is disabled or libguac lacks the video library.
 *
 * Lock order: display frame locks, then the video lock. The client's user
 * list lock is never acquired while holding either (guacd's own shutdown
 * path holds the user list lock while users leave).
 */

/**
 * Records users that joined since the last call. Must be called without
 * holding any display frame lock or the video lock.
 */
void guac_display_video_note_users(guac_display* display);

/**
 * Hands the default layer's changes in the given plan to the video encoder
 * and turns those operations into NOPs, if video is active. Also makes
 * users noted by guac_display_video_note_users() viewers, and switches
 * between video and images as users with and without video support come
 * and go.
 *
 * The caller MUST hold the write locks of both the pending and last frames.
 */
void PFW_LFW_guac_display_video_take_ops(guac_display* display,
        guac_display_plan* plan);

/**
 * Encodes and sends a video frame if one is due. Safe to call from any
 * thread that holds no display locks.
 */
void guac_display_video_encode(guac_display* display);

/**
 * Milliseconds until guac_display_video_encode() should be called even if
 * the display does not change, or -1 if there is no such deadline.
 */
int guac_display_video_next_deadline(guac_display* display);

/**
 * Forgets the given user, which is leaving the connection.
 */
void guac_display_video_user_left(guac_display* display, guac_user* user);

/**
 * Returns the socket the given layer's image updates must be written to:
 * while some users watch video and others don't, the default layer's
 * images go only to those without video; otherwise to every user (the
 * client's broadcast socket). Safe from worker threads while they hold the
 * last frame's read lock.
 */
guac_socket* guac_display_video_layer_socket(guac_display* display,
        guac_display_layer* layer);

/**
 * Frees all video state of the given display.
 */
void guac_display_video_free(guac_display* display);

#endif
