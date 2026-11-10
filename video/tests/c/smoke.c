/* SPDX-License-Identifier: Apache-2.0
 *
 * Uses the library through its C API, as guacd will: create an encoder,
 * add a viewer, push changed pixels, encode, read parts, ack, free.
 * Writes stripe 0's H.264 stream to the path in argv[1] (optional).
 * Exit status 0 means every check passed.
 */

#include "guac_extra_video.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define W 640
#define H 360
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); return 1; } } while (0)

static int log_lines = 0;

static void on_log(void* data, GxLogLevel level, const char* msg) {
    (void) data;
    log_lines++;
    fprintf(stderr, "[gx %d] %s\n", (int) level, msg);
}

static void draw(uint8_t* px, int frame) {
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            uint8_t* p = px + (y * W + x) * 4;
            int box = x >= frame * 10 && x < frame * 10 + 64 && y >= 100 && y < 164;
            p[0] = p[1] = p[2] = box ? 255 : (uint8_t) ((x ^ y) & 0x7f);
            p[3] = 255;
        }
}

int main(int argc, char** argv) {
    FILE* out = argc > 1 ? fopen(argv[1], "wb") : NULL;

    printf("guac-extra video %s\n", gx_version());

    GxConfig cfg;
    gx_config_default(&cfg);
    CHECK(cfg.fps == 60 && cfg.stripes == 1);
    cfg.stripes = 2;
    cfg.log = on_log;

    GxEncoder* enc = gx_encoder_new(&cfg, W, H, 0);
    CHECK(enc != NULL);
    CHECK(log_lines > 0); /* "video session ..." */

    CHECK(gx_encoder_stripe_count(enc) == 2);
    uint32_t sy, sh;
    CHECK(gx_encoder_stripe(enc, 0, &sy, &sh) && sy == 0 && sh == 192);
    CHECK(gx_encoder_stripe(enc, 1, &sy, &sh) && sy == 192 && sh == 168);
    CHECK(!gx_encoder_stripe(enc, 2, &sy, &sh));

    uint32_t viewer = gx_encoder_add_viewer(enc, 0);
    CHECK(viewer != 0);

    uint8_t* px = malloc(W * H * 4);
    CHECK(px != NULL);
    GxRect all = { 0, 0, W, H };
    uint64_t now = 0;
    int frames = 0;

    for (int i = 0; i < 20; i++, now += 17) {
        draw(px, i);
        gx_encoder_update(enc, px, W * 4, &all, 1);
        int r = gx_encoder_encode(enc, now);
        CHECK(r >= 0);
        if (r == 0)
            continue;

        frames++;
        uint32_t id = gx_encoder_frame_id(enc);
        CHECK(id != 0);
        if (frames == 1)
            CHECK(gx_encoder_frame_keyframe(enc));

        uint32_t who[4];
        CHECK(gx_encoder_frame_recipients(enc, who, 4) == 1 && who[0] == viewer);

        size_t n = gx_encoder_frame_parts(enc);
        CHECK(n >= 1 && n <= 2);
        for (size_t p = 0; p < n; p++) {
            GxPart part;
            CHECK(gx_encoder_frame_part(enc, p, &part));
            CHECK(part.len > 4 && part.data[0] == 0 && part.data[1] == 0);
            if (out && part.stripe == 0)
                fwrite(part.data, 1, part.len, out);
        }
        GxPart none;
        CHECK(!gx_encoder_frame_part(enc, n, &none));

        gx_encoder_ack(enc, viewer, id, now + 5);
    }

    CHECK(frames == 20);
    CHECK(gx_encoder_target_kbps(enc) > 0);
    CHECK(gx_encoder_next_deadline_ms(enc, now) >= 0); /* paint-over pending */

    gx_encoder_remove_viewer(enc, viewer);
    CHECK(gx_encoder_encode(enc, now + 1000) == 0);
    CHECK(gx_encoder_next_deadline_ms(enc, now + 1000) == -1);

    gx_encoder_free(enc);
    gx_encoder_free(NULL);
    free(px);
    if (out)
        fclose(out);
    printf("OK: %d frames\n", frames);
    return 0;
}
