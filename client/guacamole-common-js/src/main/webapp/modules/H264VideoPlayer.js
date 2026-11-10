/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole. The decoding approach
 * (WebCodecs, painting each frame as soon as it is decoded, acking decoded
 * frames) follows voxl's web/src/worker.ts (MIT, Copyright (c) 2026 Tembo).
 */

var Guacamole = Guacamole || {};

/**
 * Plays one stripe of guac-extra video (see guac-extra docs/PROTOCOL.md):
 * H.264 parts, each preceded by a 12-byte header, decoded with WebCodecs and
 * drawn onto the given layer as soon as each frame is decoded. Every decoded
 * frame is acknowledged on the stream with its frame ID, which guacd uses
 * for flow control and adaptive bitrate.
 *
 * @constructor
 * @augments Guacamole.VideoPlayer
 *
 * @param {!Guacamole.InputStream} stream
 *     The stream to read video from.
 *
 * @param {!Guacamole.Display.VisibleLayer} layer
 *     The layer to draw decoded frames onto.
 */
Guacamole.H264VideoPlayer = function H264VideoPlayer(stream, layer) {

    /**
     * Splits the stream into parts.
     *
     * @private
     * @type {!Guacamole.H264VideoPlayer.PartReader}
     */
    var reader = new Guacamole.H264VideoPlayer.PartReader();

    /**
     * The WebCodecs decoder, or null until the first keyframe (and after an
     * error).
     *
     * @private
     * @type {VideoDecoder}
     */
    var decoder = null;

    /**
     * The codec string the decoder is configured for.
     *
     * @private
     * @type {string}
     */
    var codec = null;

    /**
     * Whether any frame has been decoded. A failure before that means the
     * browser cannot play this video at all.
     *
     * @private
     * @type {!boolean}
     */
    var decodedAny = false;

    /**
     * Whether video has stopped (stream ended or failed for good).
     *
     * @private
     * @type {!boolean}
     */
    var stopped = false;

    /**
     * The latest decoded frame not yet drawn because the layer has no size
     * yet, or null.
     *
     * @private
     * @type {VideoFrame}
     */
    var pending = null;

    /**
     * When each part still being decoded was submitted (frame ID ->
     * performance.now()), for the decode-time statistic.
     *
     * @private
     * @type {!Object.<number, number>}
     */
    var submitted = {};

    /**
     * Regions of passthrough parts still being decoded (frame ID -> list),
     * and those of the frame waiting in `pending`.
     *
     * @private
     */
    var regionsById = {};
    var pendingRegions = null;

    /**
     * When each part still being decoded arrived (frame ID ->
     * performance.now()), for steady presentation.
     *
     * @private
     */
    var arrivalById = {};

    var stats = Guacamole.H264VideoPlayer.stats;
    stats.players++;

    /**
     * Draws the given frame if the layer has a size, else keeps it (newest
     * only) until it does. Takes ownership of the frame.
     *
     * @private
     * @param {!VideoFrame} frame
     */
    var draw = function draw(frame, regions) {

        if (layer.width > 0 && layer.height > 0) {

            // Passthrough pictures are valid only in their regions
            if (regions) {
                var context = layer.getCanvas().getContext('2d');
                context.save();
                context.beginPath();
                for (var i = 0; i < regions.length; i++)
                    context.rect(regions[i].x, regions[i].y,
                            regions[i].width, regions[i].height);
                context.clip();
                context.drawImage(frame, 0, 0);
                context.restore();
                frame.close();
                return;
            }

            var factor = Guacamole.H264VideoPlayer.upscaleFactor(
                    frame.displayWidth, layer.width);
            if (factor > 1)
                layer.drawImage(0, 0, upscale(frame, factor));
            else
                layer.drawImage(0, 0, frame);
            frame.close();
            return;
        }

        // The layer's "size" may not have been applied yet (it is applied
        // when the display flushes); keep the newest frame and retry
        var waiting = pending !== null;
        if (pending)
            pending.close();
        pending = frame;
        pendingRegions = regions;

        if (!waiting)
            window.setTimeout(function retry() {
                if (stopped || pending === null)
                    return;
                var next = pending;
                pending = null;
                draw(next, pendingRegions);
            }, 16);

    };

    /**
     * Canvas holding a reduced-resolution frame scaled back up, reused.
     *
     * @private
     * @type {HTMLCanvasElement}
     */
    var upscaled = null;

    /**
     * Returns the given frame scaled up by `factor` (guacd halves the
     * resolution on very slow links).
     *
     * @private
     * @param {!VideoFrame} frame
     * @param {!number} factor
     * @returns {!HTMLCanvasElement}
     */
    var upscale = function upscale(frame, factor) {
        var width = frame.displayWidth * factor;
        var height = frame.displayHeight * factor;
        if (upscaled === null)
            upscaled = document.createElement('canvas');
        if (upscaled.width !== width || upscaled.height !== height) {
            upscaled.width = width;
            upscaled.height = height;
        }
        var context = upscaled.getContext('2d');
        context.imageSmoothingQuality = 'high';
        context.drawImage(frame, 0, 0, width, height);
        return upscaled;
    };

    /**
     * Stops decoding and drops any frame not yet drawn.
     *
     * @private
     */
    var closeDecoder = function closeDecoder() {
        if (decoder && decoder.state !== 'closed') {
            try { decoder.close(); }
            catch (e) { /* already closing */ }
        }
        decoder = null;
        codec = null;
    };

    /**
     * Handles a decoder failure. Before the first decoded frame, the browser
     * cannot play this video: reply "bad type" so guacd sends images instead.
     * Later, ask for a keyframe to start over.
     *
     * @private
     * @param {*} error
     */
    var fail = function fail(error) {

        if (stopped)
            return;

        closeDecoder();

        if (!decodedAny) {
            stopped = true;
            stats.players--;
            stream.sendAck('Cannot decode H.264: ' + error,
                    Guacamole.Status.Code.CLIENT_BAD_TYPE);
        }
        else {
            stats.errors++;
            stream.sendAck('Decoder error: ' + error,
                    Guacamole.Status.Code.CLIENT_BAD_REQUEST);
        }

    };

    /**
     * Creates and configures a decoder for the given codec string.
     *
     * @private
     * @param {!string} newCodec
     * @returns {!boolean}
     *     true if the decoder is ready.
     */
    var openDecoder = function openDecoder(newCodec) {

        closeDecoder();

        try {
            decoder = new VideoDecoder({
                output: function output(frame) {
                    decodedAny = true;
                    var frameId = frame.timestamp;
                    Guacamole.H264VideoPlayer.countDecoded(frameId,
                            submitted[frameId], frame.displayWidth, frame.displayHeight);
                    delete submitted[frameId];
                    var regions = regionsById[frameId] || null;
                    delete regionsById[frameId];
                    var arrival = arrivalById[frameId];
                    delete arrivalById[frameId];
                    if (stopped)
                        frame.close();

                    // Shown at a steady pace when arrivals are uneven
                    else
                        Guacamole.H264VideoPlayer.presenter.schedule(frameId,
                                arrival !== undefined ? arrival : performance.now(),
                                function present() {
                                    if (stopped)
                                        frame.close();
                                    else
                                        draw(frame, regions);
                                });
                    stream.sendAck(String(frameId), Guacamole.Status.Code.SUCCESS);
                },
                error: function error(e) {
                    fail(e && e.message ? e.message : e);
                }
            });
            decoder.configure({
                codec: newCodec,
                optimizeForLatency: true
            });
        }
        catch (e) {
            fail(e && e.message ? e.message : e);
            return false;
        }

        codec = newCodec;
        return true;

    };

    /**
     * Decodes one part.
     *
     * @private
     * @param {!Guacamole.H264VideoPlayer.Part} part
     */
    var decode = function decode(part) {

        if (stopped)
            return;

        // A decoder starts at a keyframe; open one (or a new one if the
        // stream's profile or level changed)
        if (part.keyframe) {
            var partCodec = Guacamole.H264VideoPlayer.codecFromKeyframe(part.data);
            if (partCodec === null) {
                fail('keyframe without SPS');
                return;
            }
            if (decoder === null || partCodec !== codec) {
                if (!openDecoder(partCodec))
                    return;
            }
        }

        // Waiting for a keyframe after an error: drop delta frames
        if (decoder === null) {
            stats.dropped++;
            return;
        }

        if (part.keyframe)
            stats.keyframes++;
        submitted[part.frameId] = performance.now();
        arrivalById[part.frameId] = submitted[part.frameId];
        Guacamole.H264VideoPlayer.presenter.noteArrival(part.frameId, submitted[part.frameId]);
        if (part.regions)
            regionsById[part.frameId] = part.regions;

        try {
            decoder.decode(new EncodedVideoChunk({
                type: part.keyframe ? 'key' : 'delta',
                timestamp: part.frameId,
                data: part.data
            }));
        }
        catch (e) {
            fail(e && e.message ? e.message : e);
        }

    };

    stream.onblob = function onblob(data) {

        var binary = window.atob(data);
        stats.bytes += binary.length;
        var bytes = new Uint8Array(binary.length);
        for (var i = 0; i < binary.length; i++)
            bytes[i] = binary.charCodeAt(i);

        var parts;
        try {
            parts = reader.receive(bytes);
        }
        catch (e) {
            fail(e.message);
            return;
        }

        for (var j = 0; j < parts.length; j++)
            decode(parts[j]);

    };

    /**
     * Tells guacd whether this tab is visible. While hidden, guacd sends no
     * frames to this user (and stops encoding if nobody else watches); when
     * visible again, it sends a keyframe.
     *
     * @private
     */
    var reportVisibility = function reportVisibility() {
        if (!stopped)
            stream.sendAck(document.hidden ? 'hidden' : 'visible',
                    Guacamole.Status.Code.SUCCESS);
        stats.hidden = document.hidden;
    };

    if (typeof document !== 'undefined') {
        document.addEventListener('visibilitychange', reportVisibility);
        if (document.hidden)
            reportVisibility();
    }

    stream.onend = function onend() {
        if (!stopped)
            stats.players--;
        stopped = true;
        closeDecoder();
        if (pending) {
            pending.close();
            pending = null;
        }
        if (typeof document !== 'undefined')
            document.removeEventListener('visibilitychange', reportVisibility);
    };

    /** @override */
    this.sync = function sync() {
        // Frames are drawn as soon as they are decoded; nothing to do
    };

};

/**
 * The mimetype of guac-extra video streams.
 *
 * @constant
 * @type {!string}
 */
Guacamole.H264VideoPlayer.MIMETYPE = 'video/x-guac-extra-h264';

/**
 * Returns whether this browser can play guac-extra video: it needs
 * WebCodecs, which browsers only offer in secure contexts (HTTPS or
 * localhost). H.264 decoding itself is checked when the first keyframe
 * arrives; if it fails, the player tells guacd, which falls back to images.
 *
 * @returns {!boolean}
 */
Guacamole.H264VideoPlayer.isSupported = function isSupported() {
    return typeof window !== 'undefined'
        && window.isSecureContext !== false
        && typeof window.VideoDecoder === 'function'
        && typeof window.EncodedVideoChunk === 'function';
};

/**
 * One part of a guac-extra video stream: one encoded frame of one stripe.
 *
 * @constructor
 * @param {!number} flags
 * @param {!number} stripe
 * @param {!number} frameId
 * @param {!Uint8Array} data
 */
Guacamole.H264VideoPlayer.Part = function Part(flags, stripe, frameId, data) {

    /**
     * Whether this part is an IDR frame (a decoder can start here).
     * @type {!boolean}
     */
    this.keyframe = (flags & 0x01) !== 0;

    /**
     * Whether this part re-sends still areas at higher quality.
     * @type {!boolean}
     */
    this.paintover = (flags & 0x02) !== 0;

    /** @type {!number} */
    this.stripe = stripe;

    /** @type {!number} */
    this.frameId = frameId;

    /**
     * The parts of the decoded picture that may be drawn, or null for all
     * of it. Set when guacd forwards H.264 from the remote desktop
     * (passthrough): those pictures are only valid in these regions.
     * @type {Array.<{x: number, y: number, width: number, height: number}>}
     */
    this.regions = null;

    /**
     * H.264 Annex-B data.
     * @type {!Uint8Array}
     */
    this.data = data;

    // Region list: count, then x, y, width, height (16-bit big endian)
    if ((flags & 0x04) !== 0) {
        var view = new DataView(data.buffer, data.byteOffset, data.length);
        var count = data.length >= 2 ? view.getUint16(0) : 0;
        var end = 2 + count * 8;
        if (end > data.length)
            throw new Error('Video part region list is truncated');
        this.regions = [];
        for (var i = 0; i < count; i++) {
            var o = 2 + i * 8;
            this.regions.push({ x: view.getUint16(o), y: view.getUint16(o + 2),
                width: view.getUint16(o + 4), height: view.getUint16(o + 6) });
        }
        this.data = data.subarray(end);
    }

};

/**
 * Size of the header before each part.
 *
 * @constant
 * @type {!number}
 */
Guacamole.H264VideoPlayer.HEADER_SIZE = 12;

/**
 * Cuts a stream of bytes into parts. A part may span several blobs, and a
 * blob may hold the end of one part and the start of the next.
 *
 * @constructor
 */
Guacamole.H264VideoPlayer.PartReader = function PartReader() {

    /**
     * Bytes received but not yet returned as parts.
     *
     * @private
     * @type {!Uint8Array}
     */
    var buffer = new Uint8Array(0);

    /**
     * Adds received bytes and returns every part they complete.
     *
     * @param {!Uint8Array} bytes
     * @returns {!Guacamole.H264VideoPlayer.Part[]}
     * @throws {Error}
     *     If a header has an unknown format version.
     */
    this.receive = function receive(bytes) {

        if (buffer.length === 0)
            buffer = bytes;
        else {
            var joined = new Uint8Array(buffer.length + bytes.length);
            joined.set(buffer, 0);
            joined.set(bytes, buffer.length);
            buffer = joined;
        }

        var parts = [];
        var offset = 0;
        var HEADER = Guacamole.H264VideoPlayer.HEADER_SIZE;

        while (buffer.length - offset >= HEADER) {

            var view = new DataView(buffer.buffer, buffer.byteOffset + offset, HEADER);
            var version = view.getUint8(0);
            if (version !== 1)
                throw new Error('Unknown video format version ' + version);

            var length = view.getUint32(8);
            if (buffer.length - offset < HEADER + length)
                break;

            var start = offset + HEADER;
            parts.push(new Guacamole.H264VideoPlayer.Part(
                    view.getUint8(1), view.getUint16(2), view.getUint32(4),
                    buffer.slice(start, start + length)));

            offset = start + length;

        }

        buffer = buffer.slice(offset);
        return parts;

    };

};

/**
 * Returns the WebCodecs codec string ("avc1.PPCCLL") for the SPS in the
 * given Annex-B data, or null if it has none.
 *
 * @param {!Uint8Array} data
 * @returns {string}
 */
Guacamole.H264VideoPlayer.codecFromKeyframe = function codecFromKeyframe(data) {

    var hex = function hex(b) {
        return (b < 16 ? '0' : '') + b.toString(16);
    };

    for (var i = 0; i + 6 < data.length; i++) {

        // Start code 00 00 01 (also the tail of 00 00 00 01)
        if (data[i] !== 0 || data[i + 1] !== 0 || data[i + 2] !== 1)
            continue;

        var nal = i + 3;
        if ((data[nal] & 0x1F) === 7)
            return 'avc1.' + hex(data[nal + 1]) + hex(data[nal + 2]) + hex(data[nal + 3]);

    }

    return null;

};

/**
 * Page-wide counters of all guac-extra video players, read by the
 * statistics overlay (Guacamole.H264VideoPlayer.showStats()).
 *
 * @type {!Object}
 */
Guacamole.H264VideoPlayer.stats = {
    players: 0,       // stripes currently playing
    frames: 0,        // distinct frames decoded
    parts: 0,         // parts decoded (one per stripe per frame)
    bytes: 0,         // bytes received
    keyframes: 0,
    dropped: 0,       // parts skipped while waiting for a keyframe
    errors: 0,        // decoder failures
    decodeMs: 0,      // total time from submitting a part to its picture
    width: 0,         // size of the last decoded picture
    height: 0,
    lastFrameId: -1,
    hidden: false,
    smoothingMs: 0    // current presentation delay evening out uneven arrivals
};

/**
 * Notes one decoded part.
 *
 * @private
 * @param {!number} frameId
 * @param {number} submittedAt
 *     performance.now() when the part was submitted, if known.
 * @param {!number} width
 * @param {!number} height
 */
Guacamole.H264VideoPlayer.countDecoded = function countDecoded(frameId,
        submittedAt, width, height) {

    var stats = Guacamole.H264VideoPlayer.stats;
    stats.parts++;
    if (submittedAt !== undefined)
        stats.decodeMs += performance.now() - submittedAt;

    // Stripes of one frame share its ID; count the frame once
    if (frameId !== stats.lastFrameId) {
        stats.frames++;
        stats.lastFrameId = frameId;
    }

    stats.width = width;
    stats.height = height;

};

/**
 * Formats the change between two snapshots of the counters, taken
 * `seconds` apart, as the overlay's lines of text.
 *
 * @param {!Object} before
 * @param {!Object} after
 * @param {!number} seconds
 * @returns {!string[]}
 */
Guacamole.H264VideoPlayer.formatStats = function formatStats(before, after, seconds) {

    if (after.players <= 0)
        return [ 'Video: off (images)' ];

    var parts = after.parts - before.parts;
    var lines = [
        'Video: H.264, ' + after.players + ' stripe(s), '
            + after.width + 'x' + after.height + (after.hidden ? ' (paused: tab hidden)' : ''),
        'fps: ' + ((after.frames - before.frames) / seconds).toFixed(1),
        'kbit/s: ' + Math.round((after.bytes - before.bytes) * 8 / seconds / 1000),
        'decode: ' + (parts > 0 ? ((after.decodeMs - before.decodeMs) / parts).toFixed(1) : '-') + ' ms'
            + ', smoothing: ' + Math.round(after.smoothingMs || 0) + ' ms',
        'keyframes: ' + after.keyframes + ', dropped: ' + after.dropped
            + ', errors: ' + after.errors
    ];
    return lines;

};

/**
 * Shows or hides a small statistics box in the top right corner of the
 * page, refreshed every second.
 *
 * @param {!boolean} show
 */
Guacamole.H264VideoPlayer.showStats = (function makeShowStats() {

    var element = null;
    var timer = null;
    var previous = null;

    var snapshot = function snapshot() {
        var copy = {};
        var stats = Guacamole.H264VideoPlayer.stats;
        for (var key in stats)
            copy[key] = stats[key];
        return copy;
    };

    var refresh = function refresh() {
        var now = snapshot();
        if (previous)
            element.textContent = Guacamole.H264VideoPlayer.formatStats(previous, now, 1).join('\n');
        previous = now;
    };

    return function showStats(show) {

        if (show && element === null) {
            element = document.createElement('div');
            element.className = 'guac-extra-video-stats';
            element.style.cssText = 'position:fixed;top:8px;right:8px;z-index:10000;'
                + 'padding:6px 8px;background:rgba(0,0,0,0.7);color:#fff;'
                + 'font:12px/1.4 monospace;white-space:pre;pointer-events:none;'
                + 'border-radius:4px';
            element.textContent = 'Video: measuring...';
            document.body.appendChild(element);
            previous = snapshot();
            timer = window.setInterval(refresh, 1000);
        }

        else if (!show && element !== null) {
            window.clearInterval(timer);
            element.parentNode.removeChild(element);
            element = null;
            timer = null;
            previous = null;
        }

    };

})();

/**
 * The integer factor by which a decoded picture `frameWidth` wide must be
 * scaled to fill a layer `layerWidth` wide: 1 normally, 2 while guacd sends
 * half resolution. (Pictures may be a pixel or two wider than the layer,
 * as encoders need even sizes.)
 *
 * @param {!number} frameWidth
 * @param {!number} layerWidth
 * @returns {!number}
 */
Guacamole.H264VideoPlayer.upscaleFactor = function upscaleFactor(frameWidth, layerWidth) {
    if (frameWidth <= 0)
        return 1;
    return Math.max(1, Math.round(layerWidth / frameWidth));
};

/**
 * Shows decoded frames at a steady pace when they arrive unevenly (Wi-Fi,
 * busy links). It measures the jitter of frame arrivals and holds frames
 * for about twice that before showing them, up to `maxDelayMs`; with steady
 * arrivals the delay is zero and frames are shown as soon as they are
 * decoded. Frames of one frame ID (its stripes) are shown together, in
 * order. Shared by every player on the page.
 *
 * @constructor
 * @param {!number} maxDelayMs
 *     The most a frame may be held back.
 */
Guacamole.H264VideoPlayer.Presenter = function Presenter(maxDelayMs) {

    var presenter = this;

    /**
     * Frames waiting to be shown, in arrival order: {due, present}.
     *
     * @private
     */
    var queue = [];

    /**
     * Whether an animation frame is requested.
     *
     * @private
     */
    var scheduled = false;

    var lastFrameId = null;
    var lastArrival = null;
    var meanInterval = 0;

    /**
     * Smoothed deviation of frame intervals from their mean, in ms.
     *
     * @type {!number}
     */
    this.jitter = 0;

    /**
     * Current presentation delay, in ms.
     *
     * @type {!number}
     */
    this.delay = 0;

    /**
     * Notes that a part of the given frame arrived. Only the first part of
     * each frame counts.
     *
     * @param {!number} frameId
     * @param {!number} arrival
     *     performance.now() at arrival.
     */
    this.noteArrival = function noteArrival(frameId, arrival) {

        if (frameId === lastFrameId)
            return;

        if (lastArrival !== null) {
            var interval = arrival - lastArrival;

            // Gaps over a second are pauses (nothing changed), not jitter
            if (interval < 1000) {
                meanInterval = meanInterval === 0 ? interval : meanInterval * 0.9 + interval * 0.1;
                presenter.jitter = presenter.jitter * 0.9 + Math.abs(interval - meanInterval) * 0.1;
            }
        }

        lastFrameId = frameId;
        lastArrival = arrival;

        // Ignore small jitter: it isn't visible, and holding frames adds lag
        var target = presenter.jitter < 4 ? 0 : presenter.jitter * 2;
        presenter.delay = Math.min(maxDelayMs, target);
        Guacamole.H264VideoPlayer.stats.smoothingMs = presenter.delay;

    };

    /**
     * Shows every queued frame that is due, in order.
     *
     * @private
     */
    var flush = function flush() {
        scheduled = false;
        var now = performance.now();
        while (queue.length && queue[0].due <= now + 1)
            queue.shift().present();
        if (queue.length)
            request();
    };

    /**
     * Requests an animation frame to show due frames.
     *
     * @private
     */
    var request = function request() {
        if (scheduled)
            return;
        scheduled = true;
        window.requestAnimationFrame(flush);
    };

    /**
     * Shows a decoded frame now, or once its delay has passed.
     *
     * @param {!number} frameId
     * @param {!number} arrival
     *     When the frame's part arrived (performance.now()).
     * @param {!function} present
     *     Draws the frame.
     */
    this.schedule = function schedule(frameId, arrival, present) {

        // Nothing to even out, or no animation frames while hidden
        var hidden = typeof document !== 'undefined' && document.hidden;
        if (hidden || (presenter.delay <= 0 && queue.length === 0)) {
            present();
            return;
        }

        // Keep order: never before a frame already waiting
        var due = arrival + presenter.delay;
        if (queue.length && due < queue[queue.length - 1].due)
            due = queue[queue.length - 1].due;

        queue.push({ due: due, present: present });
        request();

    };

};

/**
 * The presenter shared by every guac-extra video player on the page.
 *
 * @type {!Guacamole.H264VideoPlayer.Presenter}
 */
Guacamole.H264VideoPlayer.presenter = new Guacamole.H264VideoPlayer.Presenter(60);
