/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 *
 * A tiny HTTP server for Prometheus. Each connection process with video
 * writes its samples (no HELP/TYPE lines) to GUACD_METRICS_DIR/<pid>.prom;
 * this server groups the samples of all live processes by metric family,
 * adds each family's HELP and TYPE once, and serves the result.
 */

#include "config.h"
#include "log.h"
#include "metrics.h"

#include <guacamole/mem.h>

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

#define GUACD_METRICS_DEFAULT_DIR "/tmp/guacd-metrics"

/**
 * Every metric family a connection process may write, with its type and
 * help text. Keep in sync with libguac/display-video.c.
 */
static const char* GUACD_METRICS_FAMILIES[][3] = {
    { "guac_extra_video_active", "gauge", "1 while the session sends video" },
    { "guac_extra_video_users", "gauge", "Users of the session by kind (video, video_low, paused, images)" },
    { "guac_extra_video_frames_total", "counter", "Video frames encoded" },
    { "guac_extra_video_keyframes_total", "counter", "Video frames that were keyframes" },
    { "guac_extra_video_bytes_total", "counter", "Bytes of H.264 encoded" },
    { "guac_extra_video_target_kbps", "gauge", "Current adaptive bitrate target (kbit/s)" },
    { "guac_extra_video_scale", "gauge", "Resolution divisor: 1 full, 2 half (slow link)" },
    { "guac_extra_video_passthrough", "gauge", "1 while the remote desktop's own H.264 is forwarded" },
    { "guac_extra_video_overlay_images_total", "counter", "Lossless overlay images sent" },
    { "guac_extra_video_fallbacks_total", "counter", "Users switched from video to images" },
    { "guac_extra_video_errors_total", "counter", "Encoding errors" },
    { NULL, NULL, NULL }
};

static const char* guacd_metrics_dir = NULL;

/**
 * Appends to a growing buffer.
 */
typedef struct guacd_metrics_buffer {
    char* data;
    size_t length;
    size_t capacity;
} guacd_metrics_buffer;

static void guacd_metrics_append(guacd_metrics_buffer* buf, const char* text, size_t length) {

    if (buf->length + length + 1 > buf->capacity) {
        size_t capacity = buf->capacity ? buf->capacity : 4096;
        while (capacity < buf->length + length + 1)
            capacity *= 2;
        buf->data = guac_mem_realloc(buf->data, capacity);
        buf->capacity = capacity;
    }

    memcpy(buf->data + buf->length, text, length);
    buf->length += length;
    buf->data[buf->length] = '\0';

}

/**
 * Reads the samples of every live connection process into `samples`,
 * removing files left by processes that are gone. Returns the number of
 * sessions read.
 */
static int guacd_metrics_read(guacd_metrics_buffer* samples) {

    DIR* dir = opendir(guacd_metrics_dir);
    if (dir == NULL)
        return 0;

    int sessions = 0;
    struct dirent* entry;
    while ((entry = readdir(dir)) != NULL) {

        int pid;
        char suffix[8];
        if (sscanf(entry->d_name, "%d.%7s", &pid, suffix) != 2
                || strcmp(suffix, "prom") != 0)
            continue;

        char path[4096];
        snprintf(path, sizeof(path), "%s/%s", guacd_metrics_dir, entry->d_name);

        /* The process ended without cleaning up */
        if (kill(pid, 0) != 0 && errno == ESRCH) {
            unlink(path);
            continue;
        }

        FILE* file = fopen(path, "r");
        if (file == NULL)
            continue;

        char chunk[4096];
        size_t length;
        while ((length = fread(chunk, 1, sizeof(chunk), file)) > 0)
            guacd_metrics_append(samples, chunk, length);

        fclose(file);
        sessions++;

    }

    closedir(dir);
    return sessions;

}

/**
 * Builds the /metrics response body.
 */
static void guacd_metrics_render(guacd_metrics_buffer* out) {

    guacd_metrics_buffer samples = { 0 };
    int sessions = guacd_metrics_read(&samples);

    char line[512];
    int n = snprintf(line, sizeof(line),
            "# HELP guac_extra_sessions Connection processes reporting video metrics\n"
            "# TYPE guac_extra_sessions gauge\n"
            "guac_extra_sessions %i\n", sessions);
    guacd_metrics_append(out, line, n);

    /* Prometheus wants each family's samples together, under one header */
    for (int f = 0; GUACD_METRICS_FAMILIES[f][0] != NULL; f++) {

        const char* name = GUACD_METRICS_FAMILIES[f][0];
        size_t name_length = strlen(name);

        n = snprintf(line, sizeof(line), "# HELP %s %s\n# TYPE %s %s\n",
                name, GUACD_METRICS_FAMILIES[f][2], name, GUACD_METRICS_FAMILIES[f][1]);
        guacd_metrics_append(out, line, n);

        const char* cursor = samples.data;
        while (cursor != NULL && *cursor != '\0') {

            const char* end = strchr(cursor, '\n');
            size_t length = end ? (size_t) (end - cursor) + 1 : strlen(cursor);

            if (length > name_length && strncmp(cursor, name, name_length) == 0
                    && (cursor[name_length] == '{' || cursor[name_length] == ' '))
                guacd_metrics_append(out, cursor, length);

            cursor += length;

        }

    }

    guac_mem_free(samples.data);

}

/**
 * Answers one HTTP request on the given socket, then closes it.
 */
static void guacd_metrics_handle(int fd) {

    /* Don't let a slow client hold the (single) server thread */
    struct timeval timeout = { 2, 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));

    char request[1024];
    ssize_t received = recv(fd, request, sizeof(request) - 1, 0);
    if (received <= 0) {
        close(fd);
        return;
    }
    request[received] = '\0';

    guacd_metrics_buffer body = { 0 };
    const char* status = "200 OK";
    const char* type = "text/plain; version=0.0.4; charset=utf-8";

    if (strncmp(request, "GET /metrics ", 13) == 0)
        guacd_metrics_render(&body);
    else if (strncmp(request, "GET /health ", 12) == 0)
        guacd_metrics_append(&body, "ok\n", 3);
    else {
        status = "404 Not Found";
        guacd_metrics_append(&body, "not found\n", 10);
    }

    char header[256];
    int n = snprintf(header, sizeof(header),
            "HTTP/1.0 %s\r\nContent-Type: %s\r\nContent-Length: %zu\r\n"
            "Connection: close\r\n\r\n", status, type, body.length);

    if (send(fd, header, n, MSG_NOSIGNAL) == n && body.length > 0)
        send(fd, body.data, body.length, MSG_NOSIGNAL);

    guac_mem_free(body.data);
    close(fd);

}

static void* guacd_metrics_thread(void* data) {

    int listen_fd = (int) (intptr_t) data;

    for (;;) {
        int fd = accept(listen_fd, NULL, NULL);
        if (fd < 0) {
            if (errno == EINTR)
                continue;
            guacd_log(GUAC_LOG_ERROR, "Metrics endpoint: accept failed: %s", strerror(errno));
            break;
        }
        guacd_metrics_handle(fd);
    }

    close(listen_fd);
    return NULL;

}

void guacd_metrics_start(void) {

    const char* port_text = getenv("GUACD_METRICS_PORT");
    if (port_text == NULL || *port_text == '\0')
        return;

    int port = atoi(port_text);
    if (port <= 0 || port > 65535) {
        guacd_log(GUAC_LOG_ERROR, "Metrics endpoint: invalid GUACD_METRICS_PORT \"%s\".", port_text);
        return;
    }

    const char* bind_host = getenv("GUACD_METRICS_BIND");
    if (bind_host == NULL || *bind_host == '\0')
        bind_host = "0.0.0.0";

    /* Tell connection processes where to write */
    guacd_metrics_dir = getenv("GUACD_METRICS_DIR");
    if (guacd_metrics_dir == NULL || *guacd_metrics_dir == '\0') {
        guacd_metrics_dir = GUACD_METRICS_DEFAULT_DIR;
        setenv("GUACD_METRICS_DIR", guacd_metrics_dir, 1);
    }
    if (mkdir(guacd_metrics_dir, 0700) != 0 && errno != EEXIST) {
        guacd_log(GUAC_LOG_ERROR, "Metrics endpoint: cannot create %s: %s",
                guacd_metrics_dir, strerror(errno));
        return;
    }

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int on = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on));

    struct sockaddr_in address = { 0 };
    address.sin_family = AF_INET;
    address.sin_port = htons(port);
    if (inet_pton(AF_INET, bind_host, &address.sin_addr) != 1
            || bind(fd, (struct sockaddr*) &address, sizeof(address)) != 0
            || listen(fd, 8) != 0) {
        guacd_log(GUAC_LOG_ERROR, "Metrics endpoint: cannot listen on %s:%i: %s",
                bind_host, port, strerror(errno));
        close(fd);
        return;
    }

    pthread_t thread;
    if (pthread_create(&thread, NULL, guacd_metrics_thread, (void*) (intptr_t) fd) != 0) {
        guacd_log(GUAC_LOG_ERROR, "Metrics endpoint: cannot start its thread.");
        close(fd);
        return;
    }
    pthread_detach(thread);

    guacd_log(GUAC_LOG_INFO, "Metrics endpoint listening on %s:%i (/metrics, /health).",
            bind_host, port);

}
