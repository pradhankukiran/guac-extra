/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 *
 * H.264 video for the default layer of a guac_display. See
 * guacamole/display-video.h for the public API and guac-extra's
 * docs/ARCHITECTURE.md and docs/PROTOCOL.md for the design.
 *
 * Overview:
 *
 * - Each connected user is an "entry". Users whose handshake lists
 *   GUAC_DISPLAY_VIDEO_MIMETYPE are "supported". A session recording that
 *   includes the display is an entry too, with no user: it receives what
 *   users receive and acknowledges every frame at once.
 * - Video is "active" while video is enabled and at least one supported
 *   user is connected. Supported users get video; the others get the
 *   default layer as images, exactly as stock Guacamole sends it, through
 *   a broadcast socket that skips video users ("mixed" sessions).
 * - While active, changes to the default layer are passed to the guac-extra
 *   video encoder, and removed from the display plan unless some users
 *   still need them as images. Each
 *   stripe of the video is drawn to its own layer, a child of the default
 *   layer, through one "video" stream per entry and stripe.
 * - Users acknowledge each decoded part with an "ack" on its stream, whose
 *   message is the frame ID. The encoder uses those acks for flow control
 *   and adaptive bitrate.
 * - Lossless overlay (optional, on by default): a transparent layer above
 *   the stripes. Once a 64x64 cell has been still for
 *   GUAC_DISPLAY_VIDEO_STILL_MS, it is sent there as PNG, so still content
 *   is pixel-exact; when it changes, it is cleared so the video shows.
 */

#include "display-plan.h"
#include "display-priv.h"
#include "display-video.h"
#include "guacamole/client.h"
#include "guacamole/display-video.h"
#include "guacamole/mem.h"
#include "guacamole/protocol.h"
#include "guacamole/socket.h"
#include "guacamole/stream.h"
#include "guacamole/timestamp.h"
#include "guacamole/user.h"

#include <cairo/cairo.h>

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifdef ENABLE_GUAC_EXTRA_VIDEO
#include <guac_extra_video.h>

/**
 * Size of the header that precedes each part of a frame within a video
 * stream (see docs/PROTOCOL.md).
 */
#define GUAC_DISPLAY_VIDEO_HEADER_SIZE 12

/**
 * Version of the stream format, stored in the first header byte.
 */
#define GUAC_DISPLAY_VIDEO_FORMAT_VERSION 1

/**
 * Header flag set when the part is an IDR frame.
 */
#define GUAC_DISPLAY_VIDEO_FLAG_KEYFRAME 0x01

/**
 * Header flag set when the part re-sends still areas at higher quality.
 */
#define GUAC_DISPLAY_VIDEO_FLAG_PAINTOVER 0x02

/**
 * Lowest bitrate the adaptive bitrate controller may choose, in kbit/s.
 */
#define GUAC_DISPLAY_VIDEO_MIN_KBPS 500

/**
 * Default most encoder threads per display (many sessions share a host;
 * see docs/decisions/0011).
 */
#define GUAC_DISPLAY_VIDEO_DEFAULT_THREADS 2

/**
 * Threads converting changed pixels to the encoder's format.
 */
#define GUAC_DISPLAY_VIDEO_CONVERT_THREADS 2

/**
 * Simulcast: a user congested this long, while another is not, moves to the
 * low tier; it tries the main tier again after this long without
 * congestion there (doubling, up to the max, when that fails within
 * GUAC_DISPLAY_VIDEO_TIER_FAILED_MS).
 */
#define GUAC_DISPLAY_VIDEO_TIER_DOWN_MS 3000
#define GUAC_DISPLAY_VIDEO_TIER_RETRY_MS 30000
#define GUAC_DISPLAY_VIDEO_TIER_RETRY_MAX_MS 300000
#define GUAC_DISPLAY_VIDEO_TIER_FAILED_MS 15000

/**
 * First frame ID of the low tier, half the 32-bit range from the main
 * tier's, so a late ack for one tier never counts for the other.
 */
#define GUAC_DISPLAY_VIDEO_LOW_FIRST_FRAME 0x80000000u

/**
 * Upper bound on recipients read per frame.
 */
#define GUAC_DISPLAY_VIDEO_MAX_RECIPIENTS 256

/**
 * Size of the square cells the lossless overlay tracks, in pixels.
 */
#define GUAC_DISPLAY_VIDEO_CELL 64

/**
 * How long a cell must be unchanged before it is sent losslessly.
 */
#define GUAC_DISPLAY_VIDEO_STILL_MS 300

/**
 * How often the recording's video streams restart with a keyframe, so a
 * player that seeks within the recording finds a stream start and a
 * keyframe at most this far ahead.
 */
#define GUAC_DISPLAY_VIDEO_RECORDING_RESTART_MS 10000

/**
 * A user (or the session recording) and its video state.
 */
typedef struct guac_display_video_entry {

    /**
     * The user, or NULL for the session recording. User entries are
     * removed when the user leaves.
     */
    guac_user* user;

    /**
     * Where this entry's video is written: the user's socket, or the
     * recording's.
     */
    guac_socket* socket;

    /**
     * Whether this entry can receive video (always true for the
     * recording).
     */
    int supported;

    /**
     * The entry's viewer ID within the encoder, or 0 if it currently
     * receives no video frames (no video, or paused).
     */
    uint32_t viewer;

    /**
     * One stream per stripe while the entry is a video user (receiving
     * video, or paused), else NULL.
     */
    guac_stream** streams;

    /**
     * Whether the user's browser reported its tab hidden: it keeps its
     * streams but gets no frames, and the encoder doesn't wait for it.
     */
    int paused;

    /**
     * The last round-trip estimate passed to the encoder.
     */
    int last_rtt;

    /**
     * Frames this user has acknowledged as decoded, for the log.
     */
    unsigned long acks;

    /**
     * When this entry last became a viewer.
     */
    guac_timestamp since;

    /**
     * Simulcast: 0 for the main encoder, 1 for the low-quality encoder that
     * slow users are moved to so they don't hold the others back.
     */
    int tier;

    /**
     * Simulcast bookkeeping: since when the user has been congested (0 if
     * not), since when it has not been congested on the low tier, when it
     * last moved up, and how long it must stay quiet before trying the main
     * tier again.
     */
    guac_timestamp congested_since;
    guac_timestamp quiet_since;
    guac_timestamp upgraded_at;
    guac_timestamp retry_ms;

    struct guac_display_video_entry* next;

} guac_display_video_entry;

/**
 * Video state of a guac_display.
 */
typedef struct guac_display_video {

    /**
     * Guards every other member. See the lock order in display-video.h.
     */
    pthread_mutex_t lock;

    guac_display* display;

    guac_display_video_settings settings;

    /**
     * Whether video may be used at all. Cleared permanently if the encoder
     * cannot be created.
     */
    int enabled;

    /**
     * The encoder while video is active, else NULL.
     */
    GxEncoder* encoder;

    /**
     * Simulcast: the low-quality encoder while some user is on tier 1,
     * else NULL, and the configuration both are created with.
     */
    GxEncoder* low;
    GxConfig config;

    /**
     * Display size the encoder was created for.
     */
    int width;
    int height;

    /**
     * Stripe layout and the layer each stripe is drawn to, while active.
     */
    int stripe_count;
    uint32_t* stripe_y;
    uint32_t* stripe_height;
    guac_layer** layers;

    /**
     * The lossless overlay layer while active (and the overlay is
     * enabled), else NULL.
     */
    guac_layer* overlay;

    /**
     * Overlay cell grid: when each cell last changed, and whether it is
     * currently shown losslessly on the overlay.
     */
    int cell_cols;
    int cell_rows;
    guac_timestamp* cell_changed;
    unsigned char* cell_overlaid;

    /**
     * Copy of the default layer (32-bit BGRX), kept while video is active
     * so overlay PNGs and a new simulcast encoder can be made without the
     * display's frame locks.
     */
    unsigned char* pixels;
    size_t pixels_stride;

    /**
     * Every connected user known to the video state, and the recording.
     */
    guac_display_video_entry* entries;

    /**
     * Broadcast socket for the default layer's images while video is active
     * and some users don't watch it: skips every user in `video_users`.
     */
    guac_socket* image_socket;

    /**
     * Whether the default layer's images currently go through
     * `image_socket` (video active, and at least one user without video).
     * Changed only while the display's frame locks are held for writing.
     */
    int split;

    /**
     * Users currently receiving video, for the image socket's filter.
     * Guarded by `filter_lock`, a leaf lock: nothing else is acquired
     * while holding it (the filter runs while user sockets are locked).
     */
    pthread_mutex_t filter_lock;
    guac_user** video_users;
    int video_user_count;

    /**
     * Counters for the metrics endpoint (see guacd/metrics.c), and when
     * they were last written, or 0. Metrics are written only if guacd set
     * GUACD_METRICS_DIR.
     */
    unsigned long frames;
    unsigned long keyframes;
    unsigned long long bytes;
    unsigned long overlay_images;
    unsigned long fallbacks;
    unsigned long errors;
    guac_timestamp metrics_written;

} guac_display_video;

/**
 * How often a connection process rewrites its metrics file.
 */
#define GUAC_DISPLAY_VIDEO_METRICS_MS 2000

/**
 * Forwards library log messages to the client log.
 */
static void guac_display_video_log(void* data, GxLogLevel level, const char* msg) {

    guac_client* client = (guac_client*) data;
    guac_client_log_level guac_level;

    switch (level) {
        case GX_LOG_LEVEL_ERROR:   guac_level = GUAC_LOG_ERROR;   break;
        case GX_LOG_LEVEL_WARNING: guac_level = GUAC_LOG_WARNING; break;
        case GX_LOG_LEVEL_INFO:    guac_level = GUAC_LOG_INFO;    break;
        default:                   guac_level = GUAC_LOG_DEBUG;   break;
    }

    guac_client_log(client, guac_level, "Video: %s", msg);

}

/**
 * Returns whether the user's handshake listed the video mimetype.
 */
static int guac_display_video_user_supported(guac_user* user) {

    const char** mimetype = user->info.video_mimetypes;
    if (mimetype == NULL)
        return 0;

    for (; *mimetype != NULL; mimetype++) {
        if (strcmp(*mimetype, GUAC_DISPLAY_VIDEO_MIMETYPE) == 0)
            return 1;
    }

    return 0;

}

static guac_display_video_entry* guac_display_video_find(
        guac_display_video* video, guac_user* user) {

    guac_display_video_entry* entry = video->entries;
    while (entry != NULL && (entry->user == NULL || entry->user != user))
        entry = entry->next;

    return entry;

}

/**
 * The encoder serving the given simulcast tier (may be NULL).
 */
static GxEncoder* guac_display_video_tier(guac_display_video* video, int tier) {
    return tier == 1 ? video->low : video->encoder;
}

/**
 * Filter for the image socket: includes users not receiving video.
 */
static int guac_display_video_image_filter(guac_user* user, void* data) {

    guac_display_video* video = (guac_display_video*) data;
    int include = 1;

    pthread_mutex_lock(&video->filter_lock);
    for (int i = 0; i < video->video_user_count; i++) {
        if (video->video_users[i] == user) {
            include = 0;
            break;
        }
    }
    pthread_mutex_unlock(&video->filter_lock);

    return include;

}

/**
 * Rebuilds the image socket's filter from the current viewers. The video
 * lock must be held, and no image may be in flight on the image socket
 * (call while holding the display's frame locks for writing).
 */
static void guac_display_video_update_filter(guac_display_video* video) {

    int count = 0;
    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {
        if (entry->user != NULL && entry->streams != NULL)
            count++;
    }

    guac_user** users = guac_mem_alloc(count > 0 ? count : 1, sizeof(guac_user*));
    int i = 0;
    for (entry = video->entries; entry != NULL; entry = entry->next) {
        if (entry->user != NULL && entry->streams != NULL)
            users[i++] = entry->user;
    }

    pthread_mutex_lock(&video->filter_lock);
    guac_user** old = video->video_users;
    video->video_users = users;
    video->video_user_count = count;
    pthread_mutex_unlock(&video->filter_lock);

    guac_mem_free(old);

}

/**
 * Flushes the socket of every entry that receives video.
 */
static void guac_display_video_flush_viewers(guac_display_video* video) {

    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {
        if (entry->viewer != 0)
            guac_socket_flush(entry->socket);
    }

}

/* ------------------------------------------------------------------------
 * Lossless overlay
 * ------------------------------------------------------------------------ */

/**
 * Calls `fn` once per rectangle covering the cells marked in `mask`
 * (horizontal runs of cells, merged with identical runs directly above),
 * in pixels and clipped to the display.
 */
static void guac_display_video_mask_rects(guac_display_video* video,
        const unsigned char* mask,
        void (*fn)(guac_display_video* video, int x, int y, int w, int h, void* data),
        void* data) {

    int cols = video->cell_cols;
    int rows = video->cell_rows;
    const int cell = GUAC_DISPLAY_VIDEO_CELL;

    /* Runs of the previous row that may still grow downwards:
     * [start column, end column, first row] */
    int* open = guac_mem_alloc(cols * 3, sizeof(int));
    int open_count = 0;

    int* next = guac_mem_alloc(cols * 3, sizeof(int));

    for (int row = 0; row <= rows; row++) {

        int next_count = 0;

        /* Runs in this row (none past the last row, which closes all) */
        int col = 0;
        while (row < rows && col < cols) {

            if (!mask[row * cols + col]) {
                col++;
                continue;
            }

            int start = col;
            while (col < cols && mask[row * cols + col])
                col++;

            /* Continue an identical run from the row above, if any */
            int first_row = row;
            for (int i = 0; i < open_count; i++) {
                if (open[i * 3] == start && open[i * 3 + 1] == col) {
                    first_row = open[i * 3 + 2];
                    open[i * 3] = -1;
                    break;
                }
            }

            next[next_count * 3] = start;
            next[next_count * 3 + 1] = col;
            next[next_count * 3 + 2] = first_row;
            next_count++;

        }

        /* Runs from the row above that did not continue are complete */
        for (int i = 0; i < open_count; i++) {

            if (open[i * 3] < 0)
                continue;

            int x = open[i * 3] * cell;
            int y = open[i * 3 + 2] * cell;
            int w = open[i * 3 + 1] * cell - x;
            int h = row * cell - y;

            if (x + w > video->width) w = video->width - x;
            if (y + h > video->height) h = video->height - y;
            if (w > 0 && h > 0)
                fn(video, x, y, w, h, data);

        }

        int* swap = open;
        open = next;
        next = swap;
        open_count = next_count;

    }

    guac_mem_free(open);
    guac_mem_free(next);

}

/**
 * guac_display_video_mask_rects() callback: clears a rectangle of the
 * overlay (makes it transparent) on one socket.
 */
static void guac_display_video_clear_rect(guac_display_video* video,
        int x, int y, int w, int h, void* data) {

    guac_socket* socket = (guac_socket*) data;
    guac_protocol_send_rect(socket, video->overlay, x, y, w, h);
    guac_protocol_send_cfill(socket, GUAC_COMP_ROUT, video->overlay,
            0x00, 0x00, 0x00, 0xFF);

}

/**
 * guac_display_video_mask_rects() callback: draws a rectangle of the
 * current screen onto the overlay, as PNG, for one entry.
 */
static void guac_display_video_paint_rect(guac_display_video* video,
        int x, int y, int w, int h, void* data) {

    guac_display_video_entry* entry = (guac_display_video_entry*) data;

    cairo_surface_t* surface = cairo_image_surface_create_for_data(
            video->pixels + y * video->pixels_stride + x * 4,
            CAIRO_FORMAT_RGB24, w, h, video->pixels_stride);

    if (entry->user != NULL)
        guac_user_stream_png(entry->user, entry->socket, GUAC_COMP_OVER,
                video->overlay, x, y, surface);
    else
        guac_client_stream_png(video->display->client, entry->socket,
                GUAC_COMP_OVER, video->overlay, x, y, surface);
    video->overlay_images++;

    cairo_surface_destroy(surface);

}

/**
 * Sends the overlay cells marked in `mask` to every entry receiving video:
 * as PNG if `paint`, else as transparent (cleared).
 */
static void guac_display_video_overlay_send(guac_display_video* video,
        const unsigned char* mask, int paint) {

    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {

        if (entry->viewer == 0)
            continue;

        if (paint)
            guac_display_video_mask_rects(video, mask,
                    guac_display_video_paint_rect, entry);
        else
            guac_display_video_mask_rects(video, mask,
                    guac_display_video_clear_rect, entry->socket);

    }

}

/**
 * Sends every cell that has been still long enough to the overlay. Returns
 * whether anything was sent.
 */
static int guac_display_video_overlay_paint_due(guac_display_video* video) {

    if (video->overlay == NULL)
        return 0;

    guac_timestamp now = guac_timestamp_current();
    int count = video->cell_cols * video->cell_rows;
    unsigned char* due = guac_mem_zalloc(count, 1);
    int any = 0;

    for (int i = 0; i < count; i++) {
        if (!video->cell_overlaid[i]
                && now - video->cell_changed[i] >= GUAC_DISPLAY_VIDEO_STILL_MS) {
            due[i] = 1;
            video->cell_overlaid[i] = 1;
            any = 1;
        }
    }

    if (any)
        guac_display_video_overlay_send(video, due, 1);

    guac_mem_free(due);
    return any;

}

/**
 * Milliseconds until the next overlay cell is due, or -1 if none.
 */
static int64_t guac_display_video_overlay_deadline(guac_display_video* video) {

    if (video->overlay == NULL)
        return -1;

    guac_timestamp now = guac_timestamp_current();
    int64_t next = -1;
    int count = video->cell_cols * video->cell_rows;

    for (int i = 0; i < count; i++) {
        if (video->cell_overlaid[i])
            continue;
        int64_t wait = video->cell_changed[i] + GUAC_DISPLAY_VIDEO_STILL_MS - now;
        if (wait < 0)
            wait = 0;
        if (next < 0 || wait < next)
            next = wait;
    }

    return next;

}

/**
 * Copies changed rectangles of the default layer into the video's own copy
 * of the screen.
 */
static void guac_display_video_copy_pixels(guac_display_video* video,
        const GxRect* rects, size_t rect_count,
        const unsigned char* buffer, size_t stride) {

    for (size_t r = 0; r < rect_count; r++) {

        int x0 = rects[r].x, y0 = rects[r].y;
        int x1 = x0 + rects[r].width, y1 = y0 + rects[r].height;
        if (x0 < 0) x0 = 0;
        if (y0 < 0) y0 = 0;
        if (x1 > video->width) x1 = video->width;
        if (y1 > video->height) y1 = video->height;

        for (int y = y0; y < y1; y++)
            memcpy(video->pixels + y * video->pixels_stride + x0 * 4,
                    buffer + y * stride + x0 * 4, (x1 - x0) * 4);

    }

}

/**
 * Notes changed rectangles of the default layer for the overlay: marks
 * their cells as changed, and clears cells that were shown losslessly.
 */
static void guac_display_video_overlay_changed(guac_display_video* video,
        const GxRect* rects, size_t rect_count,
        const unsigned char* buffer, size_t stride) {

    if (video->overlay == NULL || rect_count == 0)
        return;

    guac_timestamp now = guac_timestamp_current();
    int cols = video->cell_cols;
    int count = cols * video->cell_rows;
    unsigned char* cleared = guac_mem_zalloc(count, 1);
    int any_cleared = 0;
    const int cell = GUAC_DISPLAY_VIDEO_CELL;

    for (size_t r = 0; r < rect_count; r++) {

        int x0 = rects[r].x, y0 = rects[r].y;
        int x1 = x0 + rects[r].width, y1 = y0 + rects[r].height;
        if (x0 < 0) x0 = 0;
        if (y0 < 0) y0 = 0;
        if (x1 > video->width) x1 = video->width;
        if (y1 > video->height) y1 = video->height;
        if (x1 <= x0 || y1 <= y0)
            continue;

        for (int row = y0 / cell; row <= (y1 - 1) / cell; row++) {
            for (int col = x0 / cell; col <= (x1 - 1) / cell; col++) {
                int i = row * cols + col;
                video->cell_changed[i] = now;
                if (video->cell_overlaid[i]) {
                    video->cell_overlaid[i] = 0;
                    cleared[i] = 1;
                    any_cleared = 1;
                }
            }
        }

    }

    if (any_cleared) {
        guac_display_video_overlay_send(video, cleared, 0);
        guac_display_video_flush_viewers(video);
    }

    guac_mem_free(cleared);

}

/* ------------------------------------------------------------------------
 * Viewers
 * ------------------------------------------------------------------------ */

/* Defined below; used by the ack handler. */
static void guac_display_video_encode_locked(guac_display_video* video);

/**
 * Resumes a paused user: it becomes a viewer again (and gets a keyframe),
 * and its overlay, which missed updates while paused, is cleared and
 * repainted.
 */
static void guac_display_video_resume(guac_display_video* video,
        guac_display_video_entry* entry) {

    entry->paused = 0;
    entry->since = guac_timestamp_current();
    entry->viewer = gx_encoder_add_viewer(guac_display_video_tier(video, entry->tier), entry->since);

    if (video->overlay != NULL) {
        guac_protocol_send_rect(entry->socket, video->overlay, 0, 0,
                video->width, video->height);
        guac_protocol_send_cfill(entry->socket, GUAC_COMP_ROUT, video->overlay,
                0x00, 0x00, 0x00, 0xFF);
        guac_display_video_mask_rects(video, video->cell_overlaid,
                guac_display_video_paint_rect, entry);
    }

    guac_socket_flush(entry->socket);
    guac_user_log(entry->user, GUAC_LOG_DEBUG, "Video: resumed (tab visible).");

}

/**
 * Handles an "ack" on one of a user's video streams. The message is the ID
 * of the frame the user has decoded.
 */
static int guac_display_video_ack_handler(guac_user* user, guac_stream* stream,
        char* message, guac_protocol_status status) {

    guac_display_video* video = (guac_display_video*) stream->data;
    if (video == NULL)
        return 0;

    pthread_mutex_lock(&video->lock);

    guac_display_video_entry* entry = guac_display_video_find(video, user);
    GxEncoder* encoder = entry != NULL ? guac_display_video_tier(video, entry->tier) : NULL;
    if (entry != NULL && entry->streams != NULL && encoder != NULL) {

        /* The user's tab was hidden: stop sending it frames (and encoding,
         * if nobody else watches) until it is visible again */
        if (status == GUAC_PROTOCOL_STATUS_SUCCESS && strcmp(message, "hidden") == 0) {
            if (!entry->paused) {
                entry->paused = 1;
                gx_encoder_remove_viewer(encoder, entry->viewer);
                entry->viewer = 0;
                guac_user_log(user, GUAC_LOG_DEBUG, "Video: paused (tab hidden).");
            }
        }

        else if (status == GUAC_PROTOCOL_STATUS_SUCCESS && strcmp(message, "visible") == 0) {
            if (entry->paused)
                guac_display_video_resume(video, entry);
        }

        /* Acks below are for frames the user was sent */
        else if (entry->viewer == 0) {
        }

        /* The user cannot play this video after all: images for this
         * connection from the next frame on */
        else if (status == GUAC_PROTOCOL_STATUS_CLIENT_BAD_TYPE) {
            if (entry->supported)
                guac_user_log(user, GUAC_LOG_INFO, "Video: this user cannot "
                        "decode the video (%s); switching to images.", message);
            entry->supported = 0;
        }

        /* Frame decoded. The "OK" sent when the stream opens is not a frame
         * acknowledgement. */
        else if (status == GUAC_PROTOCOL_STATUS_SUCCESS) {
            if (message[0] >= '0' && message[0] <= '9') {
                uint32_t frame_id = (uint32_t) strtoul(message, NULL, 10);
                gx_encoder_ack(encoder, entry->viewer, frame_id,
                        guac_timestamp_current());
                entry->acks++;
            }
        }

        /* The user cannot decode this stream (e.g. its decoder failed):
         * ask for a keyframe so it can start over */
        else
            gx_encoder_request_keyframe(encoder, entry->viewer);

        /* An ack may release flow control: send a waiting frame now */
        if (gx_encoder_next_deadline_ms(encoder, guac_timestamp_current()) == 0)
            guac_display_video_encode_locked(video);

    }

    pthread_mutex_unlock(&video->lock);
    return 0;

}

static guac_stream* guac_display_video_alloc_stream(guac_display_video* video,
        guac_display_video_entry* entry) {

    if (entry->user != NULL)
        return guac_user_alloc_stream(entry->user);

    return guac_client_alloc_stream(video->display->client);

}

static void guac_display_video_free_stream(guac_display_video* video,
        guac_display_video_entry* entry, guac_stream* stream) {

    if (entry->user != NULL)
        guac_user_free_stream(entry->user, stream);
    else
        guac_client_free_stream(video->display->client, stream);

}

/**
 * Starts sending video to the given entry: creates its stripe layers (and
 * overlay) on its side, opens one stream per stripe and adds it as a
 * viewer. Video must be active.
 */
static void guac_display_video_add_viewer(guac_display_video* video,
        guac_display_video_entry* entry) {

    guac_socket* socket = entry->socket;

    guac_stream** streams = guac_mem_zalloc(video->stripe_count, sizeof(guac_stream*));
    for (int i = 0; i < video->stripe_count; i++) {

        streams[i] = guac_display_video_alloc_stream(video, entry);
        if (streams[i] == NULL) {
            guac_client_log(video->display->client, GUAC_LOG_WARNING,
                    "Video: no free streams; a %s will not receive video.",
                    entry->user != NULL ? "user" : "recording");
            for (int j = 0; j < i; j++)
                guac_display_video_free_stream(video, entry, streams[j]);
            guac_mem_free(streams);
            return;
        }

        if (entry->user != NULL) {
            streams[i]->ack_handler = guac_display_video_ack_handler;
            streams[i]->data = video;
        }

    }

    for (int i = 0; i < video->stripe_count; i++) {

        /* The stripe layer covers the display, not the even-sized padding */
        int height = video->stripe_height[i];
        if ((int) video->stripe_y[i] + height > video->height)
            height = video->height - video->stripe_y[i];

        guac_protocol_send_size(socket, video->layers[i], video->width, height);
        guac_protocol_send_move(socket, video->layers[i], GUAC_DEFAULT_LAYER,
                0, video->stripe_y[i], 0);
        guac_protocol_send_video(socket, streams[i], video->layers[i],
                GUAC_DISPLAY_VIDEO_MIMETYPE);

    }

    entry->streams = streams;
    entry->last_rtt = 0;
    entry->since = guac_timestamp_current();
    entry->tier = 0;
    entry->congested_since = 0;
    entry->retry_ms = GUAC_DISPLAY_VIDEO_TIER_RETRY_MS;
    entry->viewer = gx_encoder_add_viewer(video->encoder, entry->since);

    /* The overlay sits above the stripes; send what it currently shows */
    if (video->overlay != NULL) {
        guac_protocol_send_size(socket, video->overlay, video->width, video->height);
        guac_protocol_send_move(socket, video->overlay, GUAC_DEFAULT_LAYER, 0, 0, 1);
        guac_display_video_mask_rects(video, video->cell_overlaid,
                guac_display_video_paint_rect, entry);
    }

    guac_socket_flush(socket);

    if (entry->user != NULL)
        guac_user_log(entry->user, GUAC_LOG_INFO, "Video: sending H.264 in "
                "%i stripe(s)%s.", video->stripe_count,
                video->overlay != NULL ? " with a lossless overlay" : "");
    else
        guac_client_log(video->display->client, GUAC_LOG_DEBUG, "Video: "
                "recording H.264 in %i stripe(s).", video->stripe_count);

}

/**
 * Stops sending video to the given entry. If `notify` is set, the entry's
 * streams are ended and its stripe layers (and overlay) disposed on its
 * side.
 */
static void guac_display_video_remove_viewer(guac_display_video* video,
        guac_display_video_entry* entry, int notify) {

    if (entry->viewer == 0 && entry->streams == NULL)
        return;

    GxEncoder* encoder = guac_display_video_tier(video, entry->tier);
    if (encoder != NULL && entry->viewer != 0)
        gx_encoder_remove_viewer(encoder, entry->viewer);
    entry->viewer = 0;
    entry->paused = 0;
    entry->tier = 0;

    if (entry->streams == NULL)
        return;

    for (int i = 0; i < video->stripe_count; i++) {

        guac_stream* stream = entry->streams[i];
        stream->data = NULL;

        if (notify) {
            guac_protocol_send_end(entry->socket, stream);
            guac_protocol_send_dispose(entry->socket, video->layers[i]);
        }

        guac_display_video_free_stream(video, entry, stream);

    }

    if (notify) {
        if (video->overlay != NULL)
            guac_protocol_send_dispose(entry->socket, video->overlay);
        guac_socket_flush(entry->socket);
    }

    guac_mem_free(entry->streams);
    entry->streams = NULL;

}

/* ------------------------------------------------------------------------
 * Activation
 * ------------------------------------------------------------------------ */

/**
 * Stops video: removes every viewer, frees the encoder, stripe layers and
 * overlay.
 */
static void guac_display_video_deactivate(guac_display_video* video, int notify) {

    if (video->encoder == NULL)
        return;

    guac_client* client = video->display->client;

    guac_display_video_entry* entry = video->entries;
    while (entry != NULL) {
        guac_display_video_remove_viewer(video, entry, notify);
        entry = entry->next;
    }

    gx_encoder_free(video->encoder);
    video->encoder = NULL;
    gx_encoder_free(video->low);
    video->low = NULL;

    for (int i = 0; i < video->stripe_count; i++)
        guac_client_free_layer(client, video->layers[i]);

    if (video->overlay != NULL) {
        guac_client_free_layer(client, video->overlay);
        video->overlay = NULL;
    }

    guac_mem_free(video->layers);
    guac_mem_free(video->stripe_y);
    guac_mem_free(video->stripe_height);
    guac_mem_free(video->cell_changed);
    guac_mem_free(video->cell_overlaid);
    guac_mem_free(video->pixels);
    video->layers = NULL;
    video->stripe_y = NULL;
    video->stripe_height = NULL;
    video->cell_changed = NULL;
    video->cell_overlaid = NULL;
    video->pixels = NULL;
    video->stripe_count = 0;

}

/**
 * Starts video for a default layer of the given size whose current
 * contents are in `buffer`. Every supported entry becomes a viewer.
 */
static void guac_display_video_activate(guac_display_video* video,
        int width, int height, unsigned char* buffer, size_t stride) {

    guac_client* client = video->display->client;

    GxConfig config;
    gx_config_default(&config);
    if (video->settings.fps > 0)
        config.fps = video->settings.fps;
    if (video->settings.max_kbps > 0)
        config.max_kbps = video->settings.max_kbps;
    if (video->settings.stripes > 0)
        config.stripes = video->settings.stripes;

    /* One thread per stripe unless frames would miss their budget, up to a
     * per-session cap (voxl's default of up to 4 sliced threads per
     * session costs ~3x the CPU on desktop content) */
    config.threads = video->settings.threads > 0
        ? video->settings.threads : GUAC_DISPLAY_VIDEO_DEFAULT_THREADS;
    config.adaptive_threads = true;
    config.convert_threads = GUAC_DISPLAY_VIDEO_CONVERT_THREADS;
    config.scaling = video->settings.scaling != 0;

    /* voxl's 2000 kbit/s floor is above many home links; let the bitrate
     * controller go down to 500 kbit/s (see docs/decisions/0006) */
    config.min_kbps = GUAC_DISPLAY_VIDEO_MIN_KBPS;
    if (config.min_kbps > config.max_kbps)
        config.min_kbps = config.max_kbps;

    /* The lossless overlay shows still areas exactly; re-encoding them at
     * higher video quality (paint-over) would only cost bandwidth */
    if (video->settings.lossless)
        config.paintover_burst = 0;

    config.log = guac_display_video_log;
    config.log_data = client;
    video->config = config;

    video->encoder = gx_encoder_new(&config, width, height, guac_timestamp_current());
    if (video->encoder == NULL) {
        guac_client_log(client, GUAC_LOG_ERROR, "Video: the encoder could "
                "not be created; using images for this connection.");
        video->enabled = 0;
        return;
    }

    video->width = width;
    video->height = height;

    /* The encoder keeps its own copy of the screen: start it with all of it */
    GxRect all = { 0, 0, width, height };
    gx_encoder_update(video->encoder, buffer, stride, &all, 1);

    video->stripe_count = gx_encoder_stripe_count(video->encoder);
    video->stripe_y = guac_mem_alloc(video->stripe_count, sizeof(uint32_t));
    video->stripe_height = guac_mem_alloc(video->stripe_count, sizeof(uint32_t));
    video->layers = guac_mem_alloc(video->stripe_count, sizeof(guac_layer*));
    for (int i = 0; i < video->stripe_count; i++) {
        gx_encoder_stripe(video->encoder, i, &video->stripe_y[i], &video->stripe_height[i]);
        video->layers[i] = guac_client_alloc_layer(client);
    }

    /* Own copy of the screen (overlay images, a later simulcast encoder) */
    video->pixels_stride = (size_t) width * 4;
    video->pixels = guac_mem_alloc(height, video->pixels_stride);
    for (int y = 0; y < height; y++)
        memcpy(video->pixels + y * video->pixels_stride,
                buffer + y * stride, video->pixels_stride);

    if (video->settings.lossless) {

        const int cell = GUAC_DISPLAY_VIDEO_CELL;
        video->cell_cols = (width + cell - 1) / cell;
        video->cell_rows = (height + cell - 1) / cell;

        int count = video->cell_cols * video->cell_rows;
        guac_timestamp now = guac_timestamp_current();
        video->cell_changed = guac_mem_alloc(count, sizeof(guac_timestamp));
        video->cell_overlaid = guac_mem_zalloc(count, 1);
        for (int i = 0; i < count; i++)
            video->cell_changed[i] = now;

        video->overlay = guac_client_alloc_layer(client);

    }

    guac_display_video_entry* entry = video->entries;
    while (entry != NULL) {
        if (entry->supported)
            guac_display_video_add_viewer(video, entry);
        entry = entry->next;
    }

    guac_client_log(client, GUAC_LOG_INFO, "Video: active at %ix%i.", width, height);

}

/**
 * Stops video for one entry that stays connected and must continue with
 * images: ends its streams, disposes its stripe layers and overlay, and
 * sends it the previous frame of the default layer as one PNG (this
 * frame's image updates, which describe changes from the previous frame,
 * follow), resyncing its copy of the previous frame too.
 */
static void guac_display_video_switch_to_images(guac_display_video* video,
        guac_display_video_entry* entry) {

    guac_display_layer* layer = video->display->default_layer;
    int width = layer->last_frame.width;
    int height = layer->last_frame.height;

    guac_display_video_remove_viewer(video, entry, 1);
    if (entry->user != NULL)
        video->fallbacks++;

    if (width <= 0 || height <= 0 || layer->last_frame.buffer == NULL)
        return;

    cairo_surface_t* surface = cairo_image_surface_create_for_data(
            layer->last_frame.buffer, CAIRO_FORMAT_RGB24, width, height,
            layer->last_frame.buffer_stride);

    if (entry->user != NULL)
        guac_user_stream_png(entry->user, entry->socket, GUAC_COMP_OVER,
                GUAC_DEFAULT_LAYER, 0, 0, surface);
    else
        guac_client_stream_png(video->display->client, entry->socket,
                GUAC_COMP_OVER, GUAC_DEFAULT_LAYER, 0, 0, surface);

    guac_protocol_send_copy(entry->socket, GUAC_DEFAULT_LAYER, 0, 0,
            width, height, GUAC_COMP_OVER, layer->last_frame_buffer, 0, 0);
    guac_socket_flush(entry->socket);

    cairo_surface_destroy(surface);

}

/* ------------------------------------------------------------------------
 * Metrics
 * ------------------------------------------------------------------------ */

/**
 * Path of this process's metrics file, or NULL if metrics are off.
 */
static int guac_display_video_metrics_path(char* path, size_t size) {

    const char* dir = getenv("GUACD_METRICS_DIR");
    if (dir == NULL || *dir == '\0')
        return 0;

    snprintf(path, size, "%s/%i.prom", dir, (int) getpid());
    return 1;

}

/**
 * Rewrites this process's metrics file (samples only; guacd adds headers),
 * at most every GUAC_DISPLAY_VIDEO_METRICS_MS unless `force`. The video
 * lock must be held.
 */
static void guac_display_video_write_metrics(guac_display_video* video, int force) {

    guac_timestamp now = guac_timestamp_current();
    if (!force && video->metrics_written != 0
            && now - video->metrics_written < GUAC_DISPLAY_VIDEO_METRICS_MS)
        return;

    char path[4096];
    if (!guac_display_video_metrics_path(path, sizeof(path)))
        return;

    video->metrics_written = now;

    int video_users = 0, paused = 0, image_users = 0, low_users = 0;
    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {
        if (entry->user == NULL)
            continue;
        if (entry->streams == NULL)
            image_users++;
        else if (entry->paused)
            paused++;
        else if (entry->tier == 1)
            low_users++;
        else
            video_users++;
    }

    char tmp[4200];
    snprintf(tmp, sizeof(tmp), "%s.tmp", path);
    FILE* file = fopen(tmp, "w");
    if (file == NULL)
        return;

    const char* id = video->display->client->connection_id;
    unsigned int target = video->encoder != NULL ? gx_encoder_target_kbps(video->encoder) : 0;

    fprintf(file,
            "guac_extra_video_active{connection=\"%s\"} %i\n"
            "guac_extra_video_users{connection=\"%s\",kind=\"video\"} %i\n"
            "guac_extra_video_users{connection=\"%s\",kind=\"paused\"} %i\n"
            "guac_extra_video_users{connection=\"%s\",kind=\"images\"} %i\n"
            "guac_extra_video_users{connection=\"%s\",kind=\"video_low\"} %i\n"
            "guac_extra_video_frames_total{connection=\"%s\"} %lu\n"
            "guac_extra_video_keyframes_total{connection=\"%s\"} %lu\n"
            "guac_extra_video_bytes_total{connection=\"%s\"} %llu\n"
            "guac_extra_video_target_kbps{connection=\"%s\"} %u\n"
            "guac_extra_video_scale{connection=\"%s\"} %u\n"
            "guac_extra_video_overlay_images_total{connection=\"%s\"} %lu\n"
            "guac_extra_video_fallbacks_total{connection=\"%s\"} %lu\n"
            "guac_extra_video_errors_total{connection=\"%s\"} %lu\n",
            id, video->encoder != NULL ? 1 : 0,
            id, video_users, id, paused, id, image_users, id, low_users,
            id, video->frames, id, video->keyframes, id, video->bytes,
            id, target, id, video->encoder != NULL ? gx_encoder_scale(video->encoder) : 1,
            id, video->overlay_images, id, video->fallbacks,
            id, video->errors);

    fclose(file);
    rename(tmp, path);

}

/* ------------------------------------------------------------------------
 * Encoding and sending
 * ------------------------------------------------------------------------ */

/**
 * Sends one part of the current frame to every recipient. `buffer` must
 * have room for the header plus the part's data.
 */
static void guac_display_video_send_part(guac_display_video* video,
        int tier, const GxPart* part, uint32_t frame_id, int paintover,
        const uint32_t* recipients, size_t recipient_count,
        unsigned char* buffer) {

    size_t length = GUAC_DISPLAY_VIDEO_HEADER_SIZE + part->len;

    /* Header (big endian): version, flags, stripe, frame ID, data length */
    buffer[0] = GUAC_DISPLAY_VIDEO_FORMAT_VERSION;
    buffer[1] = (part->keyframe ? GUAC_DISPLAY_VIDEO_FLAG_KEYFRAME : 0)
              | (paintover ? GUAC_DISPLAY_VIDEO_FLAG_PAINTOVER : 0);
    buffer[2] = (part->stripe >> 8) & 0xFF;
    buffer[3] = part->stripe & 0xFF;
    buffer[4] = (frame_id >> 24) & 0xFF;
    buffer[5] = (frame_id >> 16) & 0xFF;
    buffer[6] = (frame_id >> 8) & 0xFF;
    buffer[7] = frame_id & 0xFF;
    buffer[8] = (part->len >> 24) & 0xFF;
    buffer[9] = (part->len >> 16) & 0xFF;
    buffer[10] = (part->len >> 8) & 0xFF;
    buffer[11] = part->len & 0xFF;
    memcpy(buffer + GUAC_DISPLAY_VIDEO_HEADER_SIZE, part->data, part->len);

    for (size_t r = 0; r < recipient_count; r++) {

        guac_display_video_entry* entry = video->entries;
        while (entry != NULL && (entry->tier != tier || entry->viewer != recipients[r]))
            entry = entry->next;

        if (entry == NULL || entry->streams == NULL)
            continue;

        guac_protocol_send_blobs(entry->socket,
                entry->streams[part->stripe], buffer, length);

    }

}

/**
 * Encodes and sends a frame of one simulcast tier, if one is due. Returns
 * whether anything was sent. The video lock must be held.
 */
static int guac_display_video_encode_tier(guac_display_video* video, int tier,
        guac_timestamp now) {

    GxEncoder* encoder = guac_display_video_tier(video, tier);
    if (encoder == NULL)
        return 0;

    int result = gx_encoder_encode(encoder, now);
    if (result < 0) {
        video->errors++;
        guac_client_log(video->display->client, GUAC_LOG_ERROR,
                "Video: encoding failed.");
        return 0;
    }
    if (result == 0)
        return 0;

    uint32_t frame_id = gx_encoder_frame_id(encoder);
    int paintover = gx_encoder_frame_paintover(encoder);
    video->frames++;
    video->keyframes += gx_encoder_frame_keyframe(encoder) ? 1 : 0;

    uint32_t recipients[GUAC_DISPLAY_VIDEO_MAX_RECIPIENTS];
    size_t recipient_count = gx_encoder_frame_recipients(encoder,
            recipients, GUAC_DISPLAY_VIDEO_MAX_RECIPIENTS);
    if (recipient_count > GUAC_DISPLAY_VIDEO_MAX_RECIPIENTS)
        recipient_count = GUAC_DISPLAY_VIDEO_MAX_RECIPIENTS;

    size_t parts = gx_encoder_frame_parts(encoder);
    for (size_t i = 0; i < parts; i++) {

        GxPart part;
        if (!gx_encoder_frame_part(encoder, i, &part))
            continue;

        video->bytes += part.len;
        unsigned char* buffer = guac_mem_alloc(GUAC_DISPLAY_VIDEO_HEADER_SIZE + part.len);
        guac_display_video_send_part(video, tier, &part, frame_id, paintover,
                recipients, recipient_count, buffer);
        guac_mem_free(buffer);

    }

    /* The recording (always on the main tier) "decodes" instantly: it must
     * never hold back the frames of real users */
    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {
        if (entry->user == NULL && entry->viewer != 0 && entry->tier == tier)
            gx_encoder_ack(encoder, entry->viewer, frame_id, now);
    }

    return 1;

}

/**
 * Moves a user to the given simulcast tier: off its current encoder, onto
 * the other (creating the low-tier encoder from the video's own copy of the
 * screen if needed), which sends it a keyframe.
 */
static void guac_display_video_move_tier(guac_display_video* video,
        guac_display_video_entry* entry, int tier, guac_timestamp now) {

    if (tier == 1 && video->low == NULL) {

        GxConfig config = video->config;
        config.first_frame_id = GUAC_DISPLAY_VIDEO_LOW_FIRST_FRAME;
        video->low = gx_encoder_new(&config, video->width, video->height, now);
        if (video->low == NULL)
            return;

        GxRect all = { 0, 0, video->width, video->height };
        gx_encoder_update(video->low, video->pixels, video->pixels_stride, &all, 1);

    }

    GxEncoder* from = guac_display_video_tier(video, entry->tier);
    GxEncoder* to = guac_display_video_tier(video, tier);
    if (to == NULL)
        return;

    if (from != NULL && entry->viewer != 0)
        gx_encoder_remove_viewer(from, entry->viewer);

    entry->tier = tier;
    entry->congested_since = 0;
    entry->quiet_since = now;
    entry->last_rtt = 0;
    entry->viewer = gx_encoder_add_viewer(to, now);

    guac_user_log(entry->user, GUAC_LOG_INFO, "Video: %s.", tier == 1
            ? "this user's link is slower than the others'; moved to a "
              "separate, lower-quality stream"
            : "trying the main stream again for this user");

}

/**
 * Simulcast: moves a user whose link stays congested, while another user's
 * does not, to the low tier, so it no longer holds the others back; moves
 * low-tier users back after a quiet spell. The video lock must be held.
 */
static void guac_display_video_update_tiers(guac_display_video* video,
        guac_timestamp now) {

    if (!video->settings.simulcast)
        return;

    guac_display_video_entry* entry;

    /* Main tier: who is congested, and is anyone not? */
    int main_viewers = 0, main_ok = 0;
    for (entry = video->entries; entry != NULL; entry = entry->next) {
        if (entry->user == NULL || entry->viewer == 0 || entry->tier != 0)
            continue;
        main_viewers++;
        if (gx_encoder_viewer_congested(video->encoder, entry->viewer, now)) {
            if (entry->congested_since == 0)
                entry->congested_since = now;
        }
        else {
            entry->congested_since = 0;
            main_ok++;
        }
    }

    if (main_viewers >= 2 && main_ok > 0) {
        for (entry = video->entries; entry != NULL; entry = entry->next) {
            if (entry->user == NULL || entry->viewer == 0 || entry->tier != 0
                    || entry->congested_since == 0
                    || now - entry->congested_since < GUAC_DISPLAY_VIDEO_TIER_DOWN_MS)
                continue;

            /* Back down soon after trying the main tier: wait longer next time */
            if (entry->upgraded_at != 0
                    && now - entry->upgraded_at < GUAC_DISPLAY_VIDEO_TIER_FAILED_MS) {
                entry->retry_ms *= 2;
                if (entry->retry_ms > GUAC_DISPLAY_VIDEO_TIER_RETRY_MAX_MS)
                    entry->retry_ms = GUAC_DISPLAY_VIDEO_TIER_RETRY_MAX_MS;
            }

            guac_display_video_move_tier(video, entry, 1, now);
        }
    }

    /* Low tier: back to the main tier after a quiet spell */
    int low_viewers = 0;
    for (entry = video->entries; entry != NULL; entry = entry->next) {
        if (entry->user == NULL || entry->tier != 1)
            continue;
        if (entry->viewer == 0) {
            low_viewers++; /* paused */
            continue;
        }
        if (gx_encoder_viewer_congested(video->low, entry->viewer, now))
            entry->quiet_since = now;
        if (now - entry->quiet_since >= entry->retry_ms) {
            entry->upgraded_at = now;
            guac_display_video_move_tier(video, entry, 0, now);
            continue;
        }
        low_viewers++;
    }

    /* Nobody left on the low tier: stop encoding it */
    if (low_viewers == 0 && video->low != NULL) {
        gx_encoder_free(video->low);
        video->low = NULL;
    }

}

/**
 * Encodes and sends video frames that are due (both simulcast tiers), then
 * overlay cells that are due. The video lock must be held.
 */
static void guac_display_video_encode_locked(guac_display_video* video) {

    if (video->encoder == NULL)
        return;

    guac_timestamp now = guac_timestamp_current();

    /* Restart the recording's streams now and then: a player seeking within
     * the recording restores layers but not decoder state, and needs a new
     * stream and keyframe to show video again (the keyframe goes to every
     * main-tier viewer, as the stripes are shared) */
    guac_display_video_entry* rec = video->entries;
    for (; rec != NULL; rec = rec->next) {
        if (rec->user == NULL && rec->viewer != 0
                && now - rec->since >= GUAC_DISPLAY_VIDEO_RECORDING_RESTART_MS) {
            guac_display_video_remove_viewer(video, rec, 1);
            guac_display_video_add_viewer(video, rec);
        }
    }

    /* Pass on round-trip estimates from Guacamole's own sync handling */
    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {
        if (entry->user == NULL || entry->viewer == 0)
            continue;
        int rtt = entry->user->last_frame_duration;
        GxEncoder* encoder = guac_display_video_tier(video, entry->tier);
        if (rtt > 0 && rtt != entry->last_rtt && encoder != NULL) {
            gx_encoder_rtt(encoder, entry->viewer, rtt);
            entry->last_rtt = rtt;
        }
    }

    guac_display_video_update_tiers(video, now);

    int sent = guac_display_video_encode_tier(video, 0, now);
    sent |= guac_display_video_encode_tier(video, 1, now);
    sent |= guac_display_video_overlay_paint_due(video);

    if (sent)
        guac_display_video_flush_viewers(video);

    guac_display_video_write_metrics(video, 0);

}

/* ------------------------------------------------------------------------
 * Hooks from guac_display
 * ------------------------------------------------------------------------ */

/**
 * guac_client_foreach_user() callback: adds an entry for any user not yet
 * known. Takes the video lock itself, briefly.
 */
static void* guac_display_video_note_user(guac_user* user, void* data) {

    guac_display_video* video = (guac_display_video*) data;

    pthread_mutex_lock(&video->lock);

    if (guac_display_video_find(video, user) == NULL) {

        guac_display_video_entry* entry = guac_mem_zalloc(sizeof(guac_display_video_entry));
        entry->user = user;
        entry->socket = user->socket;
        entry->supported = guac_display_video_user_supported(user);
        entry->next = video->entries;
        video->entries = entry;

        if (!entry->supported)
            guac_user_log(user, GUAC_LOG_INFO, "Video: this user does not "
                    "support \"%s\"; the connection will use images while "
                    "it is connected.", GUAC_DISPLAY_VIDEO_MIMETYPE);

    }

    pthread_mutex_unlock(&video->lock);
    return NULL;

}

void PFW_LFW_guac_display_video_take_ops(guac_display* display,
        guac_display_plan* plan) {

    guac_display_video* video = display->video;
    if (video == NULL)
        return;

    pthread_mutex_lock(&video->lock);

    guac_display_layer* layer = display->default_layer;
    int width = layer->pending_frame.width;
    int height = layer->pending_frame.height;
    unsigned char* buffer = layer->pending_frame.buffer;
    size_t stride = layer->pending_frame.buffer_stride;

    /* Video is active while at least one supported user is connected (the
     * recording follows the users, it doesn't count) */
    int video_users = 0;
    int image_users = 0;
    guac_display_video_entry* entry = video->entries;
    for (; entry != NULL; entry = entry->next) {
        if (entry->user == NULL)
            continue;
        if (entry->supported)
            video_users++;
        else
            image_users++;
    }
    int want_active = video->enabled && video_users > 0
        && width > 0 && height > 0 && buffer != NULL;

    /* Entries that can no longer take video (a user's decoder failed, or
     * video stops altogether) continue with images, from the previous frame */
    for (entry = video->entries; entry != NULL; entry = entry->next) {
        if (entry->streams != NULL && (!entry->supported || !want_active))
            guac_display_video_switch_to_images(video, entry);
    }

    int was_active = video->encoder != NULL;

    /* A new size needs a new encoder (and new stripe layers) */
    if (was_active && want_active && (width != video->width || height != video->height))
        guac_display_video_deactivate(video, 1);

    else if (was_active && !want_active) {
        guac_display_video_deactivate(video, 1);
        guac_client_log(display->client, GUAC_LOG_INFO, "Video: stopped; "
                "using images.");
    }

    if (want_active && video->encoder == NULL)
        guac_display_video_activate(video, width, height, buffer, stride);

    /* Supported entries that joined while video is active become viewers */
    if (video->encoder != NULL) {
        for (entry = video->entries; entry != NULL; entry = entry->next) {
            if (entry->streams == NULL && entry->supported)
                guac_display_video_add_viewer(video, entry);
        }
    }

    /* The image socket skips exactly the users now receiving video */
    guac_display_video_update_filter(video);
    video->split = video->encoder != NULL && image_users > 0;

    /* While active, the default layer's changes go to the encoder. They stay
     * in the plan (for the image socket) only if some users need images. */
    if (plan != NULL && video->encoder != NULL) {

        GxRect* rects = guac_mem_alloc(plan->length > 0 ? plan->length : 1, sizeof(GxRect));
        size_t rect_count = 0;

        guac_display_plan_operation* op = plan->ops;
        for (size_t i = 0; i < plan->length; i++, op++) {

            if (op->layer != layer || op->type == GUAC_DISPLAY_PLAN_OPERATION_NOP)
                continue;

            rects[rect_count].x = op->dest.left;
            rects[rect_count].y = op->dest.top;
            rects[rect_count].width = guac_rect_width(&op->dest);
            rects[rect_count].height = guac_rect_height(&op->dest);
            rect_count++;

            if (!video->split)
                op->type = GUAC_DISPLAY_PLAN_OPERATION_NOP;

        }

        if (rect_count > 0) {
            gx_encoder_update(video->encoder, buffer, stride, rects, rect_count);
            if (video->low != NULL)
                gx_encoder_update(video->low, buffer, stride, rects, rect_count);
            guac_display_video_copy_pixels(video, rects, rect_count, buffer, stride);
            guac_display_video_overlay_changed(video, rects, rect_count, buffer, stride);
        }

        guac_mem_free(rects);

    }

    guac_display_video_write_metrics(video, 0);

    pthread_mutex_unlock(&video->lock);

}

guac_socket* guac_display_video_layer_socket(guac_display* display,
        guac_display_layer* layer) {

    guac_display_video* video = display->video;
    if (video != NULL && video->split && layer == display->default_layer)
        return video->image_socket;

    return display->client->socket;

}

void guac_display_video_note_users(guac_display* display) {

    guac_display_video* video = display->video;
    if (video == NULL || !video->enabled)
        return;

    guac_client_foreach_user(display->client, guac_display_video_note_user, video);

}

void guac_display_video_encode(guac_display* display) {

    guac_display_video* video = display->video;
    if (video == NULL)
        return;

    pthread_mutex_lock(&video->lock);
    guac_display_video_encode_locked(video);
    pthread_mutex_unlock(&video->lock);

}

int guac_display_video_next_deadline(guac_display* display) {

    guac_display_video* video = display->video;
    if (video == NULL)
        return -1;

    pthread_mutex_lock(&video->lock);

    int64_t deadline = -1;
    if (video->encoder != NULL) {

        deadline = gx_encoder_next_deadline_ms(video->encoder, guac_timestamp_current());

        if (video->low != NULL) {
            int64_t low = gx_encoder_next_deadline_ms(video->low, guac_timestamp_current());
            if (low >= 0 && (deadline < 0 || low < deadline))
                deadline = low;
        }

        int64_t overlay = guac_display_video_overlay_deadline(video);
        if (overlay >= 0 && (deadline < 0 || overlay < deadline))
            deadline = overlay;

    }

    pthread_mutex_unlock(&video->lock);

    if (deadline > 1000)
        deadline = 1000;

    return (int) deadline;

}

void guac_display_video_user_left(guac_display* display, guac_user* user) {

    guac_display_video* video = display->video;
    if (video == NULL)
        return;

    pthread_mutex_lock(&video->lock);

    guac_display_video_entry** link = &video->entries;
    while (*link != NULL) {

        guac_display_video_entry* entry = *link;
        if (entry->user != NULL && entry->user == user) {

            if (entry->acks > 0)
                guac_user_log(user, GUAC_LOG_INFO, "Video: this user "
                        "decoded %lu frame(s).", entry->acks);

            /* The user is leaving: nothing more is sent to it */
            guac_display_video_remove_viewer(video, entry, 0);
            *link = entry->next;
            guac_mem_free(entry);
            break;

        }

        link = &entry->next;

    }

    pthread_mutex_unlock(&video->lock);

}

void guac_display_video_free(guac_display* display) {

    guac_display_video* video = display->video;
    if (video == NULL)
        return;

    pthread_mutex_lock(&video->lock);

    /* Users are gone by now, and the recording may already be closed:
     * release state without sending anything */
    guac_display_video_deactivate(video, 0);

    char path[4096];
    if (guac_display_video_metrics_path(path, sizeof(path)))
        unlink(path);

    guac_display_video_entry* entry = video->entries;
    while (entry != NULL) {
        guac_display_video_entry* next = entry->next;
        guac_mem_free(entry);
        entry = next;
    }

    pthread_mutex_unlock(&video->lock);
    pthread_mutex_destroy(&video->lock);

    guac_socket_free(video->image_socket);
    guac_mem_free(video->video_users);
    pthread_mutex_destroy(&video->filter_lock);

    guac_mem_free(video);
    display->video = NULL;

}

int guac_display_enable_video(guac_display* display,
        const guac_display_video_settings* settings) {

    if (display->video != NULL)
        return 0;

    guac_display_video* video = guac_mem_zalloc(sizeof(guac_display_video));
    pthread_mutex_init(&video->lock, NULL);
    pthread_mutex_init(&video->filter_lock, NULL);
    video->display = display;
    video->image_socket = guac_socket_broadcast_filtered(display->client,
            guac_display_video_image_filter, video);
    video->enabled = 1;
    if (settings != NULL)
        video->settings = *settings;

    display->video = video;

    guac_client_log(display->client, GUAC_LOG_INFO, "Video: enabled (%s, "
            "library %s, lossless overlay %s).", GUAC_DISPLAY_VIDEO_MIMETYPE,
            gx_version(), video->settings.lossless ? "on" : "off");

    return 0;

}

int guac_display_video_record(guac_display* display, guac_socket* socket) {

    guac_display_video* video = display->video;
    if (video == NULL || socket == NULL)
        return 1;

    pthread_mutex_lock(&video->lock);

    guac_display_video_entry* entry = guac_mem_zalloc(sizeof(guac_display_video_entry));
    entry->socket = socket;
    entry->supported = 1;
    entry->next = video->entries;
    video->entries = entry;

    pthread_mutex_unlock(&video->lock);

    guac_client_log(display->client, GUAC_LOG_INFO, "Video: the session "
            "recording will include the video.");

    return 0;

}

#else /* Built without the guac-extra video library */

void guac_display_video_note_users(guac_display* display) {
}

void PFW_LFW_guac_display_video_take_ops(guac_display* display,
        guac_display_plan* plan) {
}

void guac_display_video_encode(guac_display* display) {
}

int guac_display_video_next_deadline(guac_display* display) {
    return -1;
}

void guac_display_video_user_left(guac_display* display, guac_user* user) {
}

void guac_display_video_free(guac_display* display) {
}

guac_socket* guac_display_video_layer_socket(guac_display* display,
        guac_display_layer* layer) {
    return display->client->socket;
}

int guac_display_enable_video(guac_display* display,
        const guac_display_video_settings* settings) {

    guac_client_log(display->client, GUAC_LOG_WARNING, "Video was requested, "
            "but guacd was built without the guac-extra video library. "
            "Using images.");

    return 1;

}

int guac_display_video_record(guac_display* display, guac_socket* socket) {
    return 1;
}

#endif
