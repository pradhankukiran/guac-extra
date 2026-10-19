/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

/* global Guacamole, jasmine, expect */

describe('Guacamole.H264VideoPlayer', function H264VideoPlayerSpec() {

    /**
     * Builds one part: 12-byte header, then the data.
     */
    var part = function part(flags, stripe, frameId, data) {
        var bytes = new Uint8Array(12 + data.length);
        var view = new DataView(bytes.buffer);
        view.setUint8(0, 1);
        view.setUint8(1, flags);
        view.setUint16(2, stripe);
        view.setUint32(4, frameId);
        view.setUint32(8, data.length);
        bytes.set(data, 12);
        return bytes;
    };

    var concat = function concat(a, b) {
        var out = new Uint8Array(a.length + b.length);
        out.set(a, 0);
        out.set(b, a.length);
        return out;
    };

    describe('PartReader', function PartReaderSpec() {

        it('returns a part contained in one blob', function() {
            var reader = new Guacamole.H264VideoPlayer.PartReader();
            var parts = reader.receive(part(1, 2, 417, [9, 8, 7]));
            expect(parts.length).toBe(1);
            expect(parts[0].keyframe).toBe(true);
            expect(parts[0].paintover).toBe(false);
            expect(parts[0].stripe).toBe(2);
            expect(parts[0].frameId).toBe(417);
            expect(Array.from(parts[0].data)).toEqual([9, 8, 7]);
        });

        it('reassembles a part split across blobs, even inside the header', function() {
            var reader = new Guacamole.H264VideoPlayer.PartReader();
            var bytes = part(2, 0, 5, [1, 2, 3, 4, 5, 6]);
            expect(reader.receive(bytes.slice(0, 7)).length).toBe(0);
            expect(reader.receive(bytes.slice(7, 14)).length).toBe(0);
            var parts = reader.receive(bytes.slice(14));
            expect(parts.length).toBe(1);
            expect(parts[0].paintover).toBe(true);
            expect(Array.from(parts[0].data)).toEqual([1, 2, 3, 4, 5, 6]);
        });

        it('returns several parts from one blob and keeps the remainder', function() {
            var reader = new Guacamole.H264VideoPlayer.PartReader();
            var third = part(0, 0, 3, [30, 31]);
            var bytes = concat(concat(part(0, 0, 1, [10]), part(0, 0, 2, [20])), third.slice(0, 5));
            var parts = reader.receive(bytes);
            expect(parts.map(function(p) { return p.frameId; })).toEqual([1, 2]);
            parts = reader.receive(third.slice(5));
            expect(parts.length).toBe(1);
            expect(parts[0].frameId).toBe(3);
        });

        it('accepts empty parts', function() {
            var reader = new Guacamole.H264VideoPlayer.PartReader();
            var parts = reader.receive(part(0, 1, 9, []));
            expect(parts.length).toBe(1);
            expect(parts[0].data.length).toBe(0);
        });

        it('reads frame IDs above 2^31 as unsigned', function() {
            var reader = new Guacamole.H264VideoPlayer.PartReader();
            var parts = reader.receive(part(0, 0, 0xFFFFFFFE, [1]));
            expect(parts[0].frameId).toBe(4294967294);
        });

        it('rejects an unknown format version', function() {
            var reader = new Guacamole.H264VideoPlayer.PartReader();
            var bytes = part(0, 0, 1, [1]);
            bytes[0] = 2;
            expect(function() { reader.receive(bytes); }).toThrow();
        });

    });

    describe('codecFromKeyframe', function codecSpec() {

        it('reads profile, constraints and level from the SPS', function() {
            // 00 00 00 01 | SPS (type 7): High profile (100), constraints 0, level 4.0
            var data = new Uint8Array([0, 0, 0, 1, 0x67, 100, 0x00, 40, 0xAC, 0xD9]);
            expect(Guacamole.H264VideoPlayer.codecFromKeyframe(data)).toBe('avc1.640028');
        });

        it('finds the SPS after other NAL units', function() {
            var data = new Uint8Array([0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 66, 0xC0, 30, 0, 0]);
            expect(Guacamole.H264VideoPlayer.codecFromKeyframe(data)).toBe('avc1.42c01e');
        });

        it('returns null without an SPS', function() {
            var data = new Uint8Array([0, 0, 0, 1, 0x65, 1, 2, 3, 4, 5]);
            expect(Guacamole.H264VideoPlayer.codecFromKeyframe(data)).toBe(null);
        });

    });

    describe('upscaleFactor', function upscaleSpec() {

        it('is 1 for full resolution, even with padding', function() {
            expect(Guacamole.H264VideoPlayer.upscaleFactor(1024, 1024)).toBe(1);
            expect(Guacamole.H264VideoPlayer.upscaleFactor(1026, 1025)).toBe(1);
        });

        it('is 2 for half resolution', function() {
            expect(Guacamole.H264VideoPlayer.upscaleFactor(512, 1024)).toBe(2);
            expect(Guacamole.H264VideoPlayer.upscaleFactor(514, 1025)).toBe(2);
        });

    });

    describe('statistics', function statsSpec() {

        var base = function base() {
            return { players: 2, frames: 100, parts: 200, bytes: 50000, keyframes: 1,
                     dropped: 0, errors: 0, decodeMs: 400, width: 1024, height: 768,
                     lastFrameId: 100, hidden: false };
        };

        it('formats rates over the interval', function() {
            var before = base();
            var after = base();
            after.frames += 30;
            after.parts += 60;
            after.bytes += 125000;   // 1000 kbit in one second
            after.decodeMs += 120;   // 2 ms per part
            var lines = Guacamole.H264VideoPlayer.formatStats(before, after, 1);
            expect(lines[0]).toBe('Video: H.264, 2 stripe(s), 1024x768');
            expect(lines[1]).toBe('fps: 30.0');
            expect(lines[2]).toBe('kbit/s: 1000');
            expect(lines[3]).toBe('decode: 2.0 ms');
        });

        it('says when video is off or paused', function() {
            var off = base();
            off.players = 0;
            expect(Guacamole.H264VideoPlayer.formatStats(base(), off, 1)).toEqual(['Video: off (images)']);
            var hidden = base();
            hidden.hidden = true;
            expect(Guacamole.H264VideoPlayer.formatStats(base(), hidden, 1)[0]).toContain('paused');
        });

        it('counts a frame once across its stripes', function() {
            var stats = Guacamole.H264VideoPlayer.stats;
            var frames = stats.frames;
            Guacamole.H264VideoPlayer.countDecoded(900001, undefined, 640, 480);
            Guacamole.H264VideoPlayer.countDecoded(900001, undefined, 640, 480);
            Guacamole.H264VideoPlayer.countDecoded(900002, undefined, 640, 480);
            expect(stats.frames - frames).toBe(2);
        });

    });

    describe('Guacamole.VideoPlayer', function videoPlayerSpec() {

        it('lists guac-extra video exactly when WebCodecs is available', function() {
            var types = Guacamole.VideoPlayer.getSupportedTypes();
            if (Guacamole.H264VideoPlayer.isSupported())
                expect(types).toEqual(['video/x-guac-extra-h264']);
            else
                expect(types).toEqual([]);
        });

        it('rejects other mimetypes', function() {
            expect(Guacamole.VideoPlayer.isSupportedType('video/mp4')).toBe(false);
            expect(Guacamole.VideoPlayer.getInstance(null, null, 'video/mp4')).toBe(null);
        });

    });

});
