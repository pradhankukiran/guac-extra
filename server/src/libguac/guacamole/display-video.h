/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#ifndef GUAC_DISPLAY_VIDEO_H
#define GUAC_DISPLAY_VIDEO_H

/**
 * H.264 video for the default layer of a guac_display (guac-extra).
 *
 * When enabled, and while every connected user supports
 * GUAC_DISPLAY_VIDEO_MIMETYPE, changes to the default layer are sent as
 * H.264 instead of images. If any user lacks support, the display falls
 * back to images for everyone until that user leaves.
 *
 * @file display-video.h
 */

#include "display-types.h"
#include "socket-types.h"

/**
 * The mimetype a user must list in its "video" handshake instruction to
 * receive video, and the mimetype of the video streams sent. The stream
 * format is described in guac-extra's docs/PROTOCOL.md.
 */
#define GUAC_DISPLAY_VIDEO_MIMETYPE "video/x-guac-extra-h264"

/**
 * Video settings for a display. Zero means "use the default".
 */
typedef struct guac_display_video_settings {

    /**
     * Maximum frames per second.
     */
    int fps;

    /**
     * Bitrate ceiling for adaptive bitrate, in kbit/s.
     */
    int max_kbps;

    /**
     * Number of horizontal stripes, each an independent H.264 stream (1-16).
     */
    int stripes;

    /**
     * Most encoder threads this display may use. Stripes encode on one
     * thread each and only fan out while frames would miss their time
     * budget. Zero for the default (2).
     */
    int threads;

    /**
     * Non-zero to move a user whose link is much slower than the others' to
     * a separate, lower-quality encoder (simulcast), so the others keep
     * their quality.
     */
    int simulcast;

    /**
     * Non-zero to forward H.264 the remote desktop itself sends (RDP
     * Graphics Pipeline, AVC420) to video users instead of re-encoding the
     * screen, whenever that stream can stand in for guac-extra's own (see
     * guac_display_video_passthrough()).
     */
    int passthrough;

    /**
     * Non-zero to encode the area around the mouse pointer (where the user
     * is working) at better quality than the rest of the screen.
     */
    int focus;

    /**
     * Non-zero to halve the video's resolution while the link can't carry
     * even the lowest bitrate (500 kbit/s); browsers scale it back up.
     */
    int scaling;

    /**
     * Non-zero to send areas that have been still for 300 ms losslessly
     * (PNG) on a layer above the video, so still text is pixel-exact.
     */
    int lossless;

} guac_display_video_settings;

/**
 * Enables H.264 video for the default layer of the given display. Must be
 * called once, before users join and before the first frame is flushed.
 *
 * @param display
 *     The display to enable video for.
 *
 * @param settings
 *     The video settings, or NULL for defaults.
 *
 * @return
 *     Zero if video was enabled, non-zero if libguac was built without the
 *     guac-extra video library.
 */
int guac_display_enable_video(guac_display* display,
        const guac_display_video_settings* settings);

/**
 * A rectangle of the screen, in display pixels.
 */
typedef struct guac_display_video_region {
    int x;
    int y;
    int width;
    int height;
} guac_display_video_region;

/**
 * Offers one frame of H.264 that the remote desktop sent for the whole
 * screen (e.g. RDP AVC420), already decoded by the protocol plugin into the
 * display as usual. If passthrough is enabled and possible, guacd forwards
 * it to video users as is, instead of encoding the screen itself:
 *
 * - it starts only at a keyframe (IDR) from the remote desktop;
 * - it is used only with one stripe, at full resolution, and with no
 *   recording of the video;
 * - when a user needs a keyframe the remote desktop hasn't sent (joining,
 *   decoder error, tab shown again, moving between simulcast tiers), guacd
 *   goes back to its own encoder for everyone, until the next keyframe from
 *   the remote desktop.
 *
 * Call from the thread that draws the protocol's updates.
 *
 * @param display
 *     The display the frame belongs to.
 *
 * @param h264
 *     One H.264 access unit (Annex-B).
 *
 * @param length
 *     Its length in bytes.
 *
 * @param regions
 *     The parts of the picture this frame updates, in display pixels; the
 *     rest of the decoded picture must not be shown.
 *
 * @param region_count
 *     The number of regions.
 *
 * @param width
 *     Width of the picture the frame encodes (must equal the display's).
 *
 * @param height
 *     Height of the picture the frame encodes (must equal the display's).
 */
void guac_display_video_passthrough(guac_display* display,
        const unsigned char* h264, size_t length,
        const guac_display_video_region* regions, int region_count,
        int width, int height);

/**
 * Makes a session recording receive the video too: the recording socket
 * gets the same stripe layers, video streams and lossless overlay as users,
 * and acknowledges every frame at once, so it never slows users down. Call
 * after guac_display_enable_video(), only for recordings that include the
 * display.
 *
 * @param display
 *     The display whose video should be recorded.
 *
 * @param socket
 *     The recording's socket.
 *
 * @return
 *     Zero on success, non-zero if video is not enabled or unavailable.
 */
int guac_display_video_record(guac_display* display, guac_socket* socket);

#endif
