#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
End-to-end test of guac-extra video, speaking the Guacamole protocol
directly to guacd (no browser, no Java web app).

Each scenario connects one or more test clients to guacd, which connects to
the test desktop (docker/e2e.compose.yaml). A client that lists
video/x-guac-extra-h264 in its handshake should receive H.264 stripes; one
that doesn't should receive images. Every H.264 stream received is checked
with ffprobe: it must decode, and decode exactly as many frames as parts
were received.

Usage: guac_video_client.py [--guacd HOST:PORT] SCENARIO...
Scenarios: vnc-video vnc-images vnc-mixed vnc-mixed-badtype vnc-stripes vnc-paintover
           vnc-keyframe vnc-badtype rdp-video vnc-shared vnc-recording
           rdp-resize vnc-lossless vnc-hidden metrics
Benchmark: bench (video vs images, terminal and video playback, no shaping)
Network:   net-10mbit net-5mbit net-2mbit net-1mbit net-recover net-scaling
           net-simulcast (video vs images on a shaped link; needs
           the guac-extra/netem:dev image and the e2e guacd container)
Exit status 0 when every scenario passes.
"""

import argparse
import base64
import urllib.request
import codecs
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

MIME = "video/x-guac-extra-h264"
HEADER = struct.Struct(">BBHII")  # version, flags, stripe, frame id, length

VNC = {"hostname": "desktop", "port": "5900", "enable-video": "true"}
RDP = {
    "hostname": "desktop", "port": "3389", "username": "test", "password": "test",
    "security": "any", "ignore-cert": "true", "enable-video": "true",
}


class Conn:
    """A Guacamole protocol connection to guacd."""

    def __init__(self, host, port):
        self.sock = socket.create_connection((host, port), timeout=10)
        self.sock.settimeout(0.2)
        self.decoder = codecs.getincrementaldecoder("utf-8")()
        self.buf = ""
        self.bytes_in = 0

    def send(self, opcode, *args):
        parts = [opcode, *map(str, args)]
        data = ",".join(f"{len(p)}.{p}" for p in parts) + ";"
        self.sock.sendall(data.encode())

    def _parse(self):
        """One complete instruction from the buffer, or None."""
        elems, i = [], 0
        while True:
            dot = self.buf.find(".", i)
            if dot < 0:
                return None
            n = int(self.buf[i:dot])
            end = dot + 1 + n
            if end >= len(self.buf):
                return None
            elems.append(self.buf[dot + 1:end])
            term = self.buf[end]
            i = end + 1
            if term == ";":
                self.buf = self.buf[i:]
                return elems

    def read(self):
        """Next instruction as a list, or None if none arrived in 0.2 s."""
        while True:
            inst = self._parse()
            if inst is not None:
                return inst
            try:
                data = self.sock.recv(1 << 16)
            except socket.timeout:
                return None
            if not data:
                raise ConnectionError("guacd closed the connection")
            self.bytes_in += len(data)
            self.buf += self.decoder.decode(data)

    def expect(self, opcode, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            inst = self.read()
            if inst and inst[0] == opcode:
                return inst[1:]
            if inst and inst[0] == "error":
                raise RuntimeError(f"guacd error: {inst[1:]}")
        raise TimeoutError(f"no {opcode!r} from guacd")

    def close(self):
        try:
            self.send("disconnect")
        except OSError:
            pass
        self.sock.close()


class Client:
    """A test user. Records what it receives; acks video parts."""

    def __init__(self, guacd, name, video, bad_type=False):
        self.guacd, self.name, self.video = guacd, name, video
        # Answer each video stream like a browser that can't decode it
        self.bad_type = bad_type
        self.streams = {}      # video stream index -> state
        self.ended = []        # finished video streams
        self.images = {}       # layer -> img instructions
        self.image_streams = set()
        self.disposed = []
        self.events = []       # (time, what) for ordering checks
        self.stop = threading.Event()
        self.error = None
        self.actions = []      # (seconds after connect, function(client))
        self.syncs = []        # (seconds after connect, one-way delay in ms)
        self.sizes = {}        # layer -> (width, height), from "size"
        self.moves = {}        # layer -> (parent, x, y, z), from "move"
        self.layer_events = [] # (time, layer, "img" | "clear")
        self.img_data = {}     # image stream -> [layer, x, y, mimetype, bytes]
        self.finished_imgs = []  # (time, layer, x, y, mimetype, bytes)
        self.parts = []        # (time, stripe, flags) of every video part
        self.part_bytes = []   # (time, bytes of H.264) of every video part

    def connect(self, select, params):
        host, port = self.guacd.split(":")
        self.conn = c = Conn(host, int(port))
        c.send("select", select)
        args = c.expect("args")
        c.send("size", 1024, 768, 96)
        c.send("audio")
        if self.video:
            c.send("video", MIME)
        else:
            c.send("video")
        c.send("image", "image/png", "image/jpeg", "image/webp")
        c.send("timezone", "UTC")
        c.send("connect", args[0], *[params.get(n, "") for n in args[1:]])
        self.id = c.expect("ready")[0]
        self.t0 = time.time()
        return self.id

    def _event(self, what):
        self.events.append((time.time() - self.t0, what))

    def _video_blob(self, index, data):
        st = self.streams[index]
        st["buf"] += data
        buf = st["buf"]
        while len(buf) >= HEADER.size:
            version, flags, stripe, frame, length = HEADER.unpack_from(buf)
            if version != 1:
                raise ValueError(f"stream {index}: bad header version {version}")
            if len(buf) < HEADER.size + length:
                break
            if self.bad_type:
                del buf[:HEADER.size + length]
                continue
            st["h264"] += buf[HEADER.size:HEADER.size + length]
            st["parts"] += 1
            st["keyframes"] += flags & 1
            st["stripe"] = stripe
            self.parts.append((time.time() - self.t0, stripe, flags))
            self.part_bytes.append((time.time() - self.t0, length))
            del buf[:HEADER.size + length]
            self.conn.send("ack", index, frame, 0)

    def run(self, seconds):
        try:
            end = time.time() + seconds
            while time.time() < end and not self.stop.is_set():
                while self.actions and time.time() - self.t0 >= self.actions[0][0]:
                    self.actions.pop(0)[1](self)
                inst = self.conn.read()
                if inst is None:
                    continue
                op, args = inst[0], inst[1:]
                if op == "sync":
                    self.conn.send("sync", args[0])
                    # guacd stamps syncs with CLOCK_MONOTONIC in ms, the
                    # same clock as time.monotonic() on this machine
                    delay = time.monotonic() * 1000 - int(args[0])
                    self.syncs.append((time.time() - self.t0, delay))
                elif op == "video":
                    index, layer, mime = args
                    if mime != MIME:
                        raise ValueError(f"unexpected video mimetype {mime}")
                    self.streams[index] = {"layer": layer, "buf": bytearray(),
                                           "h264": bytearray(), "parts": 0,
                                           "keyframes": 0, "stripe": None}
                    self._event(f"video {index}")
                    # Like Guacamole.Client: "OK" when a player exists, else
                    # "BAD TYPE" (0x030F)
                    if self.bad_type:
                        self.conn.send("ack", index, "BAD TYPE", 0x030F)
                    else:
                        self.conn.send("ack", index, "OK", 0)
                elif op == "blob":
                    index, data = args
                    if index in self.streams:
                        self._video_blob(index, base64.b64decode(data))
                    elif index in self.img_data:
                        self.img_data[index][4] += base64.b64decode(data)
                elif op == "img":
                    index, _mask, layer = args[0], args[1], args[2]
                    self.image_streams.add(index)
                    self.img_data[index] = [layer, int(args[4]), int(args[5]), args[3], bytearray()]
                    self.images[layer] = self.images.get(layer, 0) + 1
                    self.layer_events.append((time.time() - self.t0, layer, "img"))
                    if layer == "0":
                        self._event("img 0")
                elif op == "end":
                    index = args[0]
                    if index in self.streams:
                        self.ended.append(self.streams.pop(index))
                        self._event(f"end {index}")
                    self.image_streams.discard(index)
                    if index in self.img_data:
                        layer, x, y, mime, data = self.img_data.pop(index)
                        self.finished_imgs.append((time.time() - self.t0, layer, x, y, mime, bytes(data)))
                elif op == "dispose":
                    self.disposed.append(args[0])
                elif op == "size" and len(args) >= 3:
                    self.sizes[args[0]] = (int(args[1]), int(args[2]))
                elif op == "move" and len(args) >= 5:
                    self.moves[args[0]] = tuple(int(a) for a in args[1:5])
                elif op == "cfill" and args[0] == "2":  # GUAC_COMP_ROUT (0x2): clear
                    self.layer_events.append((time.time() - self.t0, args[1], "clear"))
                elif op == "error":
                    raise RuntimeError(f"guacd error: {args}")
        except Exception as e:  # reported by the scenario
            self.error = e
        finally:
            self.conn.close()

    def all_streams(self):
        return self.ended + list(self.streams.values())


def check_decodes(client, label):
    """ffprobe every video stream; return a list of problems."""
    problems = []
    for i, st in enumerate(client.all_streams()):
        if st["parts"] == 0:
            continue
        with tempfile.NamedTemporaryFile(suffix=".h264", delete=False) as f:
            f.write(st["h264"])
            path = f.name
        try:
            out = subprocess.run(
                ["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
                 "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0", path],
                capture_output=True, text=True)
        finally:
            os.unlink(path)
        frames = out.stdout.strip()
        if out.returncode != 0 or out.stderr.strip():
            problems.append(f"{label} stream {i}: ffprobe error: {out.stderr.strip()[:200]}")
        elif frames != str(st["parts"]):
            problems.append(f"{label} stream {i}: {st['parts']} parts but {frames} decoded frames")
    return problems


def summary(c):
    s = c.all_streams()
    return (f"{len(s)} video stream(s), {sum(x['parts'] for x in s)} parts, "
            f"{sum(x['keyframes'] for x in s)} keyframes, "
            f"{sum(len(x['h264']) for x in s) // 1024} KiB H.264; "
            f"images on layer 0: {c.images.get('0', 0)}")


def scenario_video(guacd, params, seconds=8):
    c = Client(guacd, "video", video=True)
    c.connect(params["_protocol"], params)
    c.run(seconds)
    problems = [f"client error: {c.error!r}"] if c.error else []
    parts = sum(x["parts"] for x in c.all_streams())
    if not c.all_streams():
        problems.append("no video streams received")
    if parts < 20:
        problems.append(f"only {parts} video parts in {seconds} s")
    first_video = next((t for t, e in c.events if e.startswith("video")), None)
    late_imgs = [t for t, e in c.events if e == "img 0" and first_video and t > first_video + 0.5]
    if late_imgs:
        problems.append(f"{len(late_imgs)} images on layer 0 after video started")
    problems += check_decodes(c, "video")
    return problems, summary(c)


def scenario_images(guacd, params, seconds=5):
    c = Client(guacd, "images", video=False)
    c.connect(params["_protocol"], params)
    c.run(seconds)
    problems = [f"client error: {c.error!r}"] if c.error else []
    if c.all_streams():
        problems.append("received video without listing the video mimetype")
    if c.images.get("0", 0) == 0:
        problems.append("no images on layer 0")
    return problems, summary(c)


def scenario_mixed(guacd, params, b_bad_type=False):
    """A video user is joined by an images-only user, who later leaves. The
    video user keeps its video throughout; the other gets images. With
    b_bad_type, the second user lists video but its decoder fails ("BAD
    TYPE"), so it is switched to images."""
    a = Client(guacd, "a", video=True)
    conn_id = a.connect(params["_protocol"], params)
    ta = threading.Thread(target=a.run, args=(12,))
    ta.start()
    time.sleep(4)
    b = Client(guacd, "b", video=b_bad_type, bad_type=b_bad_type)
    b.connect(conn_id, params)
    b.run(4)
    ta.join()
    problems = [f"{x.name} error: {x.error!r}" for x in (a, b) if x.error]
    if len(a.all_streams()) != 1 or a.ended:
        problems.append(f"user a's video was interrupted: {len(a.all_streams())} stream(s), "
                        f"{len(a.ended)} ended")
    during = [p for p in a.parts if 4.5 < p[0] < 8]
    if len(during) < 20:
        problems.append(f"user a got only {len(during)} video parts while b was connected")
    late_imgs = [t for t, e in a.events if e == "img 0" and t > 1]
    if late_imgs:
        problems.append(f"user a got {len(late_imgs)} images on layer 0 (should be video only)")
    if b.all_streams() and not b_bad_type:
        problems.append("user b received video")
    if b_bad_type and not b.ended:
        problems.append("user b's video was not stopped after BAD TYPE")
    if b.images.get("0", 0) < 20:
        problems.append(f"user b got only {b.images.get('0', 0)} images on layer 0")
    problems += check_decodes(a, "a")
    return problems, f"a: {summary(a)} | b: {summary(b)}"


def scenario_stripes(guacd, params, seconds=6):
    """Four stripes: each its own stream; the still bottom stripe stays quiet."""
    c = Client(guacd, "stripes", video=True)
    c.connect(params["_protocol"], {**params, "video-stripes": "4"})
    c.run(seconds)
    problems = [f"client error: {c.error!r}"] if c.error else []
    by_stripe = {}
    for _, stripe, _ in c.parts:
        by_stripe[stripe] = by_stripe.get(stripe, 0) + 1
    if sorted(by_stripe) != [0, 1, 2, 3]:
        problems.append(f"expected parts for stripes 0-3, got {sorted(by_stripe)}")
    # Rows 576-767 hold no moving window: keyframe plus a few paint-overs.
    if by_stripe.get(3, 0) > 5:
        problems.append(f"still stripe 3 got {by_stripe.get(3)} parts")
    if by_stripe.get(0, 0) < 20:
        problems.append(f"moving stripe 0 got only {by_stripe.get(0, 0)} parts")
    problems += check_decodes(c, "stripes")
    return problems, f"parts per stripe {dict(sorted(by_stripe.items()))}; {summary(c)}"


def scenario_paintover(guacd, params, seconds=6):
    """A still desktop changes once (right-click menu); once it is still
    again, guacd's timer must send paint-over parts without any new change."""
    c = Client(guacd, "paintover", video=True)
    c.connect(params["_protocol"], {**params, "port": "5901", "video-lossless": "false"})
    click_at = 2.5
    c.actions = [
        (click_at, lambda cl: cl.conn.send("mouse", 500, 400, 4)),
        (click_at + 0.05, lambda cl: cl.conn.send("mouse", 500, 400, 0)),
    ]
    c.run(seconds)
    problems = [f"client error: {c.error!r}"] if c.error else []
    after = [(t, f) for t, _, f in c.parts if t >= click_at]
    motion = [t for t, f in after if not f & 2 and not f & 1]
    paint = [t for t, f in after if f & 2]
    if not motion:
        problems.append("the click produced no video")
    elif not [t for t in paint if t >= motion[-1] + 0.14]:
        problems.append("no paint-over after the screen went still")
    if len(after) > 15:
        problems.append(f"{len(after)} parts for one click: the still screen keeps sending")
    problems += check_decodes(c, "paintover")
    return problems, (f"after click: {len(motion)} motion part(s), {len(paint)} paint-over "
                      f"part(s); {summary(c)}")


def scenario_keyframe(guacd, params, seconds=6):
    """A viewer reporting a decode error (ack with an error status) gets a
    fresh keyframe."""
    c = Client(guacd, "keyframe", video=True)
    c.connect(params["_protocol"], params)
    fail_at = 3.0
    def fail(cl):
        index = next(iter(cl.streams))
        cl.conn.send("ack", index, "decode failed", 0x0100)
    c.actions = [(fail_at, fail)]
    c.run(seconds)
    problems = [f"client error: {c.error!r}"] if c.error else []
    keys = [t for t, _, f in c.parts if f & 1]
    if not [t for t in keys if t > fail_at]:
        problems.append(f"no keyframe after the error (keyframes at {keys})")
    problems += check_decodes(c, "keyframe")
    return problems, f"keyframes at {[round(t, 2) for t in keys]} s; {summary(c)}"


def scenario_badtype(guacd, params, seconds=5):
    """A client that lists the mimetype but answers "BAD TYPE" (as the
    browser player does when it cannot decode H.264) gets images."""
    c = Client(guacd, "badtype", video=True, bad_type=True)
    c.connect(params["_protocol"], params)
    c.run(seconds)
    problems = [f"client error: {c.error!r}"] if c.error else []
    ends = [t for t, e in c.events if e.startswith("end")]
    imgs = [t for t, e in c.events if e == "img 0"]
    if not ends:
        problems.append("video was not stopped after BAD TYPE")
    elif len([t for t in imgs if t > ends[0]]) < 10:
        problems.append("few or no images after video stopped")
    starts = [t for t, e in c.events if e.startswith("video")]
    if len(starts) > 1:
        problems.append("video restarted for a client that cannot decode it")
    return problems, f"video stopped at {[round(t, 2) for t in ends]} s; {summary(c)}"


GUACD_CONTAINER = "guac-extra-e2e-guacd-1"


def shape(rate, delay_ms):
    """Limits guacd's traffic to clients (source port 4822) to `rate` with
    `delay_ms` of one-way delay; other traffic (to the desktop) is not
    touched. rate=None removes the shaping."""
    tc = ["docker", "run", "--rm", "--network", f"container:{GUACD_CONTAINER}",
          "--cap-add", "NET_ADMIN", "guac-extra/netem:dev", "sh", "-c"]
    if rate is None:
        cmd = "tc qdisc del dev eth0 root 2>/dev/null; true"
    else:
        cmd = (
            "tc qdisc add dev eth0 root handle 1: prio bands 3 "
            "priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 && "
            f"tc qdisc add dev eth0 parent 1:3 handle 30: netem delay {delay_ms}ms "
            f"rate {rate} limit 1000 && "
            "tc filter add dev eth0 protocol ip parent 1:0 prio 1 u32 "
            "match ip sport 4822 0xffff flowid 1:3"
        )
    subprocess.run(tc + [cmd], check=True, capture_output=True)


def percentile(values, p):
    v = sorted(values)
    return v[min(len(v) - 1, int(len(v) * p))] if v else float("nan")


def guacd_cpu_us():
    """CPU time used by everything in the guacd container, in microseconds."""
    out = subprocess.run(["docker", "exec", GUACD_CONTAINER, "cat", "/sys/fs/cgroup/cpu.stat"],
                         capture_output=True, text=True, check=True).stdout
    return int(next(l.split()[1] for l in out.splitlines() if l.startswith("usage_usec")))


def encoder_cpu_ticks():
    """CPU time (clock ticks) used so far by guacd's video encoder threads
    (named gx-enc-N), summed over all guacd processes."""
    out = subprocess.run(["docker", "exec", GUACD_CONTAINER, "sh", "-c",
                          "for t in /proc/[0-9]*/task/*; do "
                          "grep -q '^gx-enc' $t/comm 2>/dev/null && cat $t/stat; done"],
                         capture_output=True, text=True).stdout
    total = 0
    for line in out.splitlines():
        fields = line.rsplit(")", 1)[1].split()
        total += int(fields[11]) + int(fields[12])  # utime, stime
    return total


def measure(guacd, params, video, seconds, warmup=4):
    """Runs one client; returns frame rate, received kbit/s and the one-way
    delay of frames (p50, p95, max) after the warm-up."""
    c = Client(guacd, "net", video=video)
    c.connect(params["_protocol"], params)
    cpu0 = guacd_cpu_us()
    c.run(seconds)
    cpu = (guacd_cpu_us() - cpu0) / 1e6 / seconds
    if c.error:
        raise c.error
    span = seconds - warmup
    delays = [d for t, d in c.syncs if t >= warmup]
    if video:
        frames = len({(round(t, 3)) for t, _, _ in c.parts if t >= warmup})
    else:
        frames = len(delays)
    return {
        "fps": frames / span,
        "kbps": c.conn.bytes_in * 8 / seconds / 1000,
        "p50": percentile(delays, 0.5),
        "p95": percentile(delays, 0.95),
        "max": max(delays) if delays else float("nan"),
        "cpu": cpu,
    }


def scenario_network(guacd, rate, delay_ms, link_kbps, seconds=20):
    """Video and stock images on the same shaped link. Video must keep the
    queue short (p95 delay within 250 ms of the link's own delay) and stay
    under the link rate."""
    params = {**VNC, "_protocol": "vnc"}
    results = {}
    try:
        shape(rate, delay_ms)
        results["video"] = measure(guacd, params, True, seconds)
        results["images"] = measure(guacd, {**params, "enable-video": "false"}, False, seconds)
    finally:
        shape(None, 0)
    v = results["video"]
    problems = []
    if v["p95"] > delay_ms + 250:
        problems.append(f"video p95 delay {v['p95']:.0f} ms on a {delay_ms} ms link")
    if v["fps"] < 15:
        problems.append(f"video at {v['fps']:.1f} fps")
    if v["kbps"] > link_kbps * 1.05:
        problems.append(f"video received at {v['kbps']:.0f} kbit/s on a {link_kbps} kbit/s link")
    info = " | ".join(
        f"{k}: {r['fps']:.1f} fps, {r['kbps']:.0f} kbit/s, delay p50 {r['p50']:.0f} / "
        f"p95 {r['p95']:.0f} / max {r['max']:.0f} ms" for k, r in results.items())
    return problems, info


def scenario_recover(guacd, seconds=36):
    """The link is 1 Mbit/s for 12 s, then unlimited. The bitrate must climb
    back once the link allows it (measured 10-24 s after the change: if the
    last congestion falls just before the change, the controller keeps that
    rate as the ceiling for 8 s and probes up from there)."""
    params = {**VNC, "_protocol": "vnc"}
    half = 12
    c = Client(guacd, "recover", video=True)
    marks = {}

    def note(cl, key):
        marks[key] = cl.conn.bytes_in

    c.actions = [(4, lambda cl: note(cl, "a")), (half, lambda cl: note(cl, "b")),
                 (half, lambda cl: shape(None, 0)), (half + 10, lambda cl: note(cl, "c"))]
    try:
        shape("1mbit", 80)
        c.connect(params["_protocol"], params)
        c.run(seconds)
    finally:
        shape(None, 0)
    if c.error:
        return [f"client error: {c.error!r}"], ""
    slow = (marks["b"] - marks["a"]) * 8 / (half - 4) / 1000
    fast = (c.conn.bytes_in - marks["c"]) * 8 / (seconds - half - 10) / 1000
    problems = []
    if slow > 1050:
        problems.append(f"{slow:.0f} kbit/s received on a 1 Mbit/s link")
    if fast < slow * 1.5:
        problems.append(f"bitrate did not recover: {slow:.0f} -> {fast:.0f} kbit/s")
    return problems, f"1 Mbit/s: {slow:.0f} kbit/s, then unlimited: {fast:.0f} kbit/s"


def stream_width(st):
    """Width of the decoded video in a stream, from ffprobe."""
    with tempfile.NamedTemporaryFile(suffix=".h264", delete=False) as f:
        f.write(st["h264"])
        path = f.name
    try:
        out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0",
                              "-show_entries", "stream=width,height", "-of", "csv=p=0", path],
                             capture_output=True, text=True)
    finally:
        os.unlink(path)
    return out.stdout.strip()


def scenario_shared(guacd, params):
    """Two video users share a session: the second starts on a keyframe,
    the first keeps its stream, and both decode everything."""
    a = Client(guacd, "a", video=True)
    conn_id = a.connect(params["_protocol"], params)
    ta = threading.Thread(target=a.run, args=(9,))
    ta.start()
    time.sleep(3)
    b = Client(guacd, "b", video=True)
    b.connect(conn_id, params)
    b.run(5)
    ta.join()
    problems = [f"{x.name} error: {x.error!r}" for x in (a, b) if x.error]
    if len(a.all_streams()) != 1:
        problems.append(f"user a got {len(a.all_streams())} streams (expected to keep 1)")
    if not b.parts:
        problems.append("user b got no video")
    elif not b.parts[0][2] & 1:
        problems.append("user b's first part is not a keyframe")
    if len([p for p in a.parts if p[0] > 3]) < 20:
        problems.append("user a's video stalled after b joined")
    problems += check_decodes(a, "a") + check_decodes(b, "b")
    return problems, f"a: {summary(a)} | b: {summary(b)}"


def recording_frames(name):
    """Renders a recording in the guacd container with guacenc; returns
    (frame count, [mean luma of frames at 1 s and 4 s], problem or None)."""
    enc = subprocess.run(["docker", "exec", GUACD_CONTAINER, "/opt/guacamole/bin/guacenc",
                          "-s", "1024x768", "-f", f"/tmp/rec/{name}"],
                         capture_output=True, text=True)
    if enc.returncode != 0:
        return 0, [], f"guacenc failed: {enc.stderr.strip()[:200]}"
    with tempfile.TemporaryDirectory() as d:
        subprocess.run(["docker", "cp", f"{GUACD_CONTAINER}:/tmp/rec/{name}.m4v", d], check=True)
        path = os.path.join(d, f"{name}.m4v")
        out = subprocess.run(["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
                              "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0", path],
                             capture_output=True, text=True)
        frames = int(out.stdout.strip() or 0)
        shots = []
        for t in ("1", "4"):
            raw = subprocess.run(["ffmpeg", "-v", "error", "-ss", t, "-i", path, "-frames:v", "1",
                                  "-f", "rawvideo", "-pix_fmt", "gray", "-"],
                                 capture_output=True).stdout
            shots.append(raw)
    return frames, shots, None


def scenario_recording(guacd, params):
    """A recording of the display gets the video too: users get video, and
    guacenc renders the recording with motion. A recording of input only
    keeps video as well."""
    rec = {"recording-path": "/tmp/rec", "create-recording-path": "true",
           "recording-name": "e2e"}
    problems = []
    subprocess.run(["docker", "exec", GUACD_CONTAINER, "rm", "-rf", "/tmp/rec"], check=False)

    c = Client(guacd, "rec", video=True)
    c.connect(params["_protocol"], {**params, **rec})
    c.run(13)
    if c.error:
        problems.append(f"client error: {c.error!r}")
    if not c.all_streams():
        problems.append("no video for a user of a recorded connection")

    time.sleep(1)
    grep = subprocess.run(["docker", "exec", GUACD_CONTAINER, "sh", "-c",
                           "grep -o video/x-guac-extra-h264 /tmp/rec/e2e | wc -l"],
                          capture_output=True, text=True)
    starts = int(grep.stdout.strip() or 0)
    if starts == 0:
        problems.append("the recording has no video stream")
    elif starts < 2:
        problems.append("the recording's video streams did not restart within 13 s "
                        "(players seeking need a restart every 10 s)")
    user_keys = [t for t, _, f in c.parts if f & 1]
    if len(user_keys) < 2:
        problems.append(f"expected a keyframe from the recording restart, got {user_keys}")

    frames, shots, err = recording_frames("e2e")
    if err:
        problems.append(err)
    elif frames < 50:
        problems.append(f"recording played back as {frames} frames")
    elif len(shots) == 2 and shots[0] and shots[1]:
        mean = [sum(s) / len(s) for s in shots]
        if min(mean) < 5:
            problems.append(f"playback is black (mean luma {mean})")
        if shots[0] == shots[1]:
            problems.append("playback is frozen: frames at 1 s and 4 s are identical")

    d = Client(guacd, "rec-no-output", video=True)
    d.connect(params["_protocol"], {**params, **rec, "recording-name": "e2e-input",
                                    "recording-exclude-output": "true"})
    d.run(4)
    if not d.all_streams():
        problems.append("video was not used for a recording without the display")
    luma = [round(sum(s) / len(s)) for s in shots if s]
    return problems, (f"user: {summary(c)}; recording: {starts} stream start(s), playback "
                      f"{frames} frames, mean luma at 1 s / 4 s {luma} | input only: {summary(d)}")


def scenario_resize(guacd, params):
    """The browser window changes size; the RDP desktop follows, and video
    restarts at the new size."""
    c = Client(guacd, "resize", video=True)
    c.connect(params["_protocol"], {**params, "resize-method": "display-update"})
    c.actions = [(5, lambda cl: cl.conn.send("size", 1280, 720, 96))]
    c.run(14)
    problems = [f"client error: {c.error!r}"] if c.error else []
    streams = c.all_streams()
    widths = [stream_width(st) for st in streams if st["parts"]]
    if not widths or not widths[0].startswith("1024,"):
        problems.append(f"first video was not 1024 wide: {widths}")
    if not any(w.startswith("1280,72") for w in widths[1:]):
        problems.append(f"no video at 1280x720 after the resize: {widths}")
    problems += check_decodes(c, "resize")
    return problems, f"video sizes {widths}; {summary(c)}"


def scenario_bench(guacd, seconds=15):
    """Video vs stock images on a terminal and on video playback, with no
    link limits. Prints a Markdown table; fails only if a mode gets no
    frames."""
    rows, problems = [], []
    for content, port in (("Scrolling terminal", "5900"), ("Video playback", "5902")):
        for mode, video in (("video", True), ("images", False)):
            params = {**VNC, "_protocol": "vnc", "port": port,
                      "enable-video": "true" if video else "false"}
            r = measure(guacd, params, video, seconds)
            if r["fps"] < 1:
                problems.append(f"{content}, {mode}: no frames")
            rows.append(f"| {content} | {mode} | {r['fps']:.1f} | {r['kbps'] / 1000:.1f} | "
                        f"{r['p50']:.0f} / {r['p95']:.0f} | {r['cpu']:.2f} |")
    table = "\n".join(["", "| Content | Mode | fps | Mbit/s | Delay p50 / p95 (ms) | guacd CPU (cores) |",
                       "|---|---|---|---|---|---|", *rows])
    return problems, table


DESKTOP_CONTAINER = "guac-extra-e2e-desktop-1"


def decode_rgb(png):
    """(width, height, rgb24 bytes) of an image, via ffmpeg."""
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
        f.write(png)
        path = f.name
    try:
        wh = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=width,height",
                             "-of", "csv=p=0", path], capture_output=True, text=True).stdout
        w, h = (int(v) for v in wh.strip().split(","))
        rgb = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-f", "rawvideo",
                              "-pix_fmt", "rgb24", "-"], capture_output=True).stdout
    finally:
        os.unlink(path)
    return w, h, rgb


def screenshot(display, width=1024, height=768):
    """The test desktop's X display as rgb24 bytes, straight from Xvfb,
    without the mouse pointer (guacd never draws it into the screen)."""
    return subprocess.run(["docker", "exec", DESKTOP_CONTAINER, "ffmpeg", "-v", "error",
                           "-f", "x11grab", "-draw_mouse", "0",
                           "-video_size", f"{width}x{height}", "-i", display,
                           "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"],
                          capture_output=True, check=True).stdout


def compare_region(png, x, y, screen, width=1024):
    """Number of pixels of `png` placed at (x, y) that differ from the screen."""
    w, h, rgb = decode_rgb(png)
    diff = 0
    for row in range(h):
        a = rgb[row * w * 3:(row + 1) * w * 3]
        start = ((y + row) * width + x) * 3
        b = screen[start:start + w * 3]
        if a != b:
            diff += sum(1 for i in range(0, len(a), 3) if a[i:i + 3] != b[i:i + 3])
    return diff, w * h


def scenario_lossless(guacd, params):
    """Lossless overlay on a still desktop: painted once still, cleared when
    the screen changes (right-click menu), painted again once still; and
    absent with video-lossless=false."""
    c = Client(guacd, "lossless", video=True)
    c.connect(params["_protocol"], {**params, "port": "5901"})
    click_at = 2.5
    c.actions = [(click_at, lambda cl: cl.conn.send("mouse", 500, 400, 4)),
                 (click_at + 0.05, lambda cl: cl.conn.send("mouse", 500, 400, 0))]
    c.run(5)
    problems = [f"client error: {c.error!r}"] if c.error else []
    overlay = [l for l, m in c.moves.items() if m[0] == 0 and m[3] == 1]
    if len(overlay) != 1:
        problems.append(f"expected one overlay layer (child of 0, z 1), got {overlay}")
        return problems, summary(c)
    ov = overlay[0]
    ev = [(t, e) for t, l, e in c.layer_events if l == ov]
    before = [t for t, e in ev if e == "img" and t < click_at]
    clears = [t for t, e in ev if e == "clear" and t >= click_at]
    after = [t for t, e in ev if e == "img" and clears and t > clears[0]]
    if not before:
        problems.append("still desktop was never painted losslessly")
    if not clears:
        problems.append("the overlay was not cleared where the screen changed")
    elif not after:
        problems.append("changed cells were not painted again once still")
    elif after[0] - clears[0] < 0.25:
        problems.append(f"repainted {after[0] - clears[0]:.2f} s after the change (< 0.3 s still)")
    paint = [p for p in c.parts if p[2] & 2]
    if paint:
        problems.append(f"{len(paint)} paint-over parts although the overlay replaces them")

    # Pixel-exact: the overlay PNG painted after the screen settled must
    # match the desktop's actual pixels (the open menu stays put).
    exact = ""
    repaint = [i for i in c.finished_imgs if i[1] == ov and clears and i[0] > clears[0]]
    if repaint:
        _, _, x, y, mime, png = repaint[-1]
        if mime != "image/png":
            problems.append(f"overlay image is {mime}, not PNG")
        diff, total = compare_region(png, x, y, screenshot(":2"))
        exact = f"; repaint at ({x},{y}): {total - diff}/{total} pixels exact"
        if diff:
            problems.append(f"overlay differs from the screen in {diff} of {total} pixels")
    problems += check_decodes(c, "lossless")

    off = Client(guacd, "lossless-off", video=True)
    off.connect(params["_protocol"], {**params, "port": "5901", "video-lossless": "false"})
    off.run(3)
    if [l for l, m in off.moves.items() if m[0] == 0 and m[3] == 1]:
        problems.append("overlay layer present with video-lossless=false")
    return problems, (f"overlay layer {ov}: {len(before)} PNG(s) while still, "
                      f"{len(clears)} clear(s) after the click, {len(after)} PNG(s) once "
                      f"still again{exact}; {summary(c)}")


def scenario_hidden(guacd, params):
    """A user whose tab is hidden gets no frames (and guacd stops encoding
    when nobody watches); when visible again it gets a keyframe. Another
    user watching meanwhile is unaffected."""
    a = Client(guacd, "a", video=True)
    conn_id = a.connect(params["_protocol"], params)
    send_all = lambda msg: (lambda cl: [cl.conn.send("ack", i, msg, 0) for i in cl.streams])
    a.actions = [(3, send_all("hidden")), (7, send_all("visible"))]
    cpu = {}
    def mark(key):
        return lambda cl: cpu.__setitem__(key, (guacd_cpu_us(), encoder_cpu_ticks()))
    a.actions += [(0.5, mark("v0")), (2.5, mark("v1")), (3.5, mark("h0")), (6.5, mark("h1"))]
    a.actions.sort(key=lambda x: x[0])
    a.run(10)
    problems = [f"client error: {a.error!r}"] if a.error else []
    hidden = [p for p in a.parts if 3.3 < p[0] < 7]
    after = [p for p in a.parts if p[0] > 7]
    if hidden:
        problems.append(f"{len(hidden)} parts sent while the tab was hidden")
    if not after:
        problems.append("no video after the tab became visible")
    elif not after[0][2] & 1:
        problems.append("first part after becoming visible is not a keyframe")
    idle = enc = None
    if len(cpu) == 4:
        visible = cpu["v1"][1] - cpu["v0"][1]
        if visible == 0:
            problems.append("encoder threads not found (0 ticks while watched)")
        idle = (cpu["h1"][0] - cpu["h0"][0]) / 1e6 / 3
        enc = cpu["h1"][1] - cpu["h0"][1]
        # The encoder must be idle; guacd itself keeps decoding VNC
        if enc > 2:
            problems.append(f"encoder threads used {enc} ticks while nobody watched")

    # Two users: one hides, the other keeps getting video
    b = Client(guacd, "b", video=True)
    conn_id = b.connect(params["_protocol"], params)
    tb = threading.Thread(target=b.run, args=(8,))
    tb.start()
    time.sleep(1)
    c = Client(guacd, "c", video=True)
    c.connect(conn_id, params)
    c.actions = [(1.5, send_all("hidden"))]
    c.run(5)
    tb.join()
    if b.error or c.error:
        problems.append(f"errors: {b.error!r} {c.error!r}")
    b_during = [p for p in b.parts if 3 < p[0] < 6]
    c_hidden = [p for p in c.parts if p[0] > 1.8]
    if len(b_during) < 20:
        problems.append(f"the visible user got only {len(b_during)} parts while the other was hidden")
    if c_hidden:
        problems.append(f"the hidden user got {len(c_hidden)} parts")
    cpu_s = (f", encoder ticks while watched {visible} / hidden {enc}, rest of guacd "
             f"{idle:.2f} cores while hidden" if idle is not None else "")
    return problems, (f"parts while hidden: {len(hidden)}, after visible: {len(after)}"
                      f"{cpu_s} | shared: visible user {len(b_during)} parts, hidden user "
                      f"{len(c_hidden)} parts")


METRICS_URL = "http://127.0.0.1:9123"


def scrape():
    """{(metric, labels): value} from guacd's /metrics, and the raw text."""
    text = urllib.request.urlopen(METRICS_URL + "/metrics", timeout=5).read().decode()
    samples = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        name_labels, value = line.rsplit(" ", 1)
        samples[name_labels] = float(value)
    return samples, text


def scenario_metrics(guacd, params):
    """guacd's /metrics reports a live mixed session (one video user, one
    images user) and drops it once the session ends."""
    problems = []
    health = urllib.request.urlopen(METRICS_URL + "/health", timeout=5).read().decode().strip()
    if health != "ok":
        problems.append(f"/health said {health!r}")

    a = Client(guacd, "a", video=True)
    conn_id = a.connect(params["_protocol"], params)
    ta = threading.Thread(target=a.run, args=(8,))
    ta.start()
    time.sleep(2)
    b = Client(guacd, "b", video=False)
    b.connect(conn_id, params)
    tb = threading.Thread(target=b.run, args=(5,))
    tb.start()
    time.sleep(4)
    samples, text = scrape()
    tb.join()
    ta.join()

    for family in ("guac_extra_video_active", "guac_extra_video_frames_total"):
        if text.count(f"# TYPE {family} ") != 1:
            problems.append(f"{family} must have exactly one TYPE line")
    key = lambda name, extra="": f'{name}{{connection="{conn_id}"{extra}}}'
    want = {
        key("guac_extra_video_active"): lambda v: v == 1,
        key("guac_extra_video_users", ',kind="video"'): lambda v: v == 1,
        key("guac_extra_video_users", ',kind="images"'): lambda v: v == 1,
        key("guac_extra_video_frames_total"): lambda v: v > 50,
        key("guac_extra_video_keyframes_total"): lambda v: v >= 1,
        key("guac_extra_video_bytes_total"): lambda v: v > 10000,
        key("guac_extra_video_target_kbps"): lambda v: v >= 500,
    }
    for k, ok in want.items():
        if k not in samples:
            problems.append(f"missing {k}")
        elif not ok(samples[k]):
            problems.append(f"unexpected {k} = {samples[k]}")

    time.sleep(3)
    after, _ = scrape()
    if any(conn_id in k for k in after):
        problems.append("the ended session is still reported")
    frames = samples.get(key("guac_extra_video_frames_total"), 0)
    return problems, (f"live: {sum(1 for k in samples if conn_id in k)} samples, "
                      f"{frames:.0f} frames, sessions={samples.get('guac_extra_sessions')}; "
                      f"after end: sessions={after.get('guac_extra_sessions')}")


def frame_widths(st):
    """Width of every decoded frame of a stream (ffprobe -show_frames)."""
    with tempfile.NamedTemporaryFile(suffix=".h264", delete=False) as f:
        f.write(st["h264"])
        path = f.name
    try:
        out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0",
                              "-show_entries", "frame=width", "-of", "csv=p=0", path],
                             capture_output=True, text=True).stdout
    finally:
        os.unlink(path)
    return [int(w) for w in out.split() if w.strip().isdigit()]


def scenario_scaling(guacd, seconds=40, tail=15):
    """On a 300 kbit/s link (below the 500 kbit/s floor) video drops to half
    resolution; compared with video-scaling=false on the same link. The
    scaled session is recorded, and the recording must play back."""
    params = {**VNC, "_protocol": "vnc"}
    rec = {"recording-path": "/tmp/rec", "create-recording-path": "true",
           "recording-name": "scaled"}
    subprocess.run(["docker", "exec", GUACD_CONTAINER, "rm", "-rf", "/tmp/rec"], check=False)
    results, problems, scale_metric = {}, [], None
    try:
        shape("300kbit", 100)
        for label, extra in (("scaling", rec), ("no scaling", {"video-scaling": "false"})):
            c = Client(guacd, label, video=True)
            conn_id = c.connect("vnc", {**params, **extra})
            if label == "scaling":
                def check_metric(cl):
                    nonlocal scale_metric
                    samples, _ = scrape()
                    scale_metric = samples.get(f'guac_extra_video_scale{{connection="{conn_id}"}}')
                c.actions = [(seconds - 2, check_metric)]
            c.run(seconds)
            if c.error:
                problems.append(f"{label}: client error: {c.error!r}")
            widths = [w for st in c.all_streams() for w in frame_widths(st)]
            late = [t for t, _, _ in c.parts if t >= seconds - tail]
            delays = [d for t, d in c.syncs if t >= seconds - tail]
            results[label] = {
                "widths": sorted(set(widths)),
                "fps": len(set(round(t, 3) for t in late)) / tail,
                "p95": percentile(delays, 0.95),
            }
    finally:
        shape(None, 0)
    s, n = results.get("scaling"), results.get("no scaling")
    if s and 512 not in s["widths"]:
        problems.append(f"never switched to half resolution (widths {s['widths']})")
    if n and 512 in n["widths"]:
        problems.append("switched to half resolution with video-scaling=false")
    if scale_metric != 2:
        problems.append(f"metrics reported scale {scale_metric}, not 2")
    # The point of scaling: the stream fits the link again
    if s and n and not (s["p95"] < n["p95"] and s["fps"] >= n["fps"] * 1.5):
        problems.append("scaling did not improve delay and frame rate on the slow link")
    frames, shots, err = recording_frames("scaled")
    if err:
        problems.append(err)
    elif frames < 50 or not shots or not shots[-1] or sum(shots[-1]) / len(shots[-1]) < 5:
        problems.append(f"the scaled recording played back badly ({frames} frames)")
    info = " | ".join(f"{k}: widths {r['widths']}, {r['fps']:.1f} fps, p95 delay {r['p95']:.0f} ms "
                      f"(last {tail} s)" for k, r in results.items())
    return problems, info + f" | recording: {frames} frames"


def guacd_client_ports():
    """Remote ports of guacd's current client connections (to port 4822),
    as seen inside guacd's network namespace."""
    out = subprocess.run(["docker", "run", "--rm", "--network", f"container:{GUACD_CONTAINER}",
                          "guac-extra/netem:dev", "ss", "-Htn", "state", "established",
                          "( sport = :4822 )"], capture_output=True, text=True).stdout
    ports = set()
    for line in out.splitlines():
        peer = line.split()[-1]
        ports.add(int(peer.rsplit(":", 1)[1]))
    return ports


def shape_port(port, rate, delay_ms):
    """Limits guacd's traffic to one client connection (by its remote port)."""
    cmd = (
        "tc qdisc add dev eth0 root handle 1: prio bands 3 "
        "priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 && "
        f"tc qdisc add dev eth0 parent 1:3 handle 30: netem delay {delay_ms}ms "
        f"rate {rate} limit 1000 && "
        "tc filter add dev eth0 protocol ip parent 1:0 prio 1 u32 "
        f"match ip sport 4822 0xffff match ip dport {port} 0xffff flowid 1:3"
    )
    subprocess.run(["docker", "run", "--rm", "--network", f"container:{GUACD_CONTAINER}",
                    "--cap-add", "NET_ADMIN", "guac-extra/netem:dev", "sh", "-c", cmd],
                   check=True, capture_output=True)


def scenario_simulcast(guacd, seconds=40, window=(20, 38)):
    """Two video users share a session; one has a 300 kbit/s link. Without
    simulcast the shared bitrate controller follows the slow user, so the
    fast user's video also drops to a low bitrate and half resolution. With
    simulcast the slow user moves to its own stream and the fast user keeps
    full resolution and a normal bitrate."""
    params = {**VNC, "_protocol": "vnc"}
    results, problems = {}, []
    for label, extra in (("simulcast", {}), ("one stream", {"video-simulcast": "false"})):
        a = Client(guacd, "fast", video=True)
        conn_id = a.connect("vnc", {**params, **extra})
        ta = threading.Thread(target=a.run, args=(seconds,))
        ta.start()
        time.sleep(1)
        try:
            before = guacd_client_ports()
            b = Client(guacd, "slow", video=True)
            b.connect(conn_id, {**params, **extra})
            new = guacd_client_ports() - before
            if len(new) != 1:
                problems.append(f"{label}: could not find the slow user's connection ({new})")
                b.close() if hasattr(b, "close") else None
                ta.join()
                continue
            shape_port(new.pop(), "300kbit", 100)
            b.run(seconds - 3)
        finally:
            shape(None, 0)
        ta.join()
        for x in (a, b):
            if x.error:
                problems.append(f"{label} {x.name}: {x.error!r}")
        lo, hi = window
        fast_parts = [t for t, _, _ in a.parts if lo <= t < hi]
        fast_delay = [d for t, d in a.syncs if lo <= t < hi]
        fast_kbps = sum(n for t, n in a.part_bytes if lo <= t < hi) * 8 / (hi - lo) / 1000
        fast_widths = [w for st in a.all_streams() for w in frame_widths(st)]
        results[label] = {
            "fast_kbps": fast_kbps,
            "fast_half": 512 in fast_widths,
            "fast_fps": len(set(round(t, 3) for t in fast_parts)) / (hi - lo),
            "fast_p95": percentile(fast_delay, 0.95),
            "slow_widths": sorted(set(w for st in b.all_streams() for w in frame_widths(st))),
            "slow_keys": len([1 for _, _, f in b.parts if f & 1]),
        }
        problems += check_decodes(a, f"{label} fast") + check_decodes(b, f"{label} slow")
    s, o = results.get("simulcast"), results.get("one stream")
    if s and o:
        if s["fast_half"]:
            problems.append("with simulcast the fast user still dropped to half resolution")
        if not o["fast_half"]:
            problems.append("without simulcast the fast user kept full resolution "
                            "(the test link did not hold it back)")
        if s["fast_kbps"] < 1.5 * o["fast_kbps"]:
            problems.append(f"the fast user's video bitrate was not spared: {s['fast_kbps']:.0f} "
                            f"vs {o['fast_kbps']:.0f} kbit/s without simulcast")
        if s["fast_fps"] < 0.8 * o["fast_fps"]:
            problems.append("simulcast cost the fast user frame rate")
        if s["slow_keys"] < 2:
            problems.append("the slow user never moved to its own stream (no second keyframe)")
    info = " | ".join(f"{k}: fast user {r['fast_fps']:.1f} fps, {r['fast_kbps']:.0f} kbit/s, "
                      f"half resolution: {r['fast_half']}, p95 {r['fast_p95']:.0f} ms; "
                      f"slow user widths {r['slow_widths']}, {r['slow_keys']} keyframe(s)"
                      for k, r in results.items())
    return problems, info


SCENARIOS = {
    "vnc-video": lambda g: scenario_video(g, {**VNC, "_protocol": "vnc"}),
    "vnc-images": lambda g: scenario_images(g, {**VNC, "_protocol": "vnc"}),
    "vnc-mixed": lambda g: scenario_mixed(g, {**VNC, "_protocol": "vnc"}),
    "vnc-mixed-badtype": lambda g: scenario_mixed(g, {**VNC, "_protocol": "vnc"}, b_bad_type=True),
    "vnc-stripes": lambda g: scenario_stripes(g, {**VNC, "_protocol": "vnc"}),
    "vnc-paintover": lambda g: scenario_paintover(g, {**VNC, "_protocol": "vnc"}),
    "vnc-keyframe": lambda g: scenario_keyframe(g, {**VNC, "_protocol": "vnc"}),
    "vnc-badtype": lambda g: scenario_badtype(g, {**VNC, "_protocol": "vnc"}),
    "rdp-video": lambda g: scenario_video(g, {**RDP, "_protocol": "rdp"}, seconds=12),
    "vnc-shared": lambda g: scenario_shared(g, {**VNC, "_protocol": "vnc"}),
    "vnc-recording": lambda g: scenario_recording(g, {**VNC, "_protocol": "vnc"}),
    "rdp-resize": lambda g: scenario_resize(g, {**RDP, "_protocol": "rdp"}),
    "vnc-lossless": lambda g: scenario_lossless(g, {**VNC, "_protocol": "vnc"}),
    "vnc-hidden": lambda g: scenario_hidden(g, {**VNC, "_protocol": "vnc"}),
    "metrics": lambda g: scenario_metrics(g, {**VNC, "_protocol": "vnc"}),
    "net-10mbit": lambda g: scenario_network(g, "10mbit", 40, 10000),
    "net-5mbit": lambda g: scenario_network(g, "5mbit", 60, 5000),
    "net-2mbit": lambda g: scenario_network(g, "2mbit", 60, 2000),
    "net-1mbit": lambda g: scenario_network(g, "1mbit", 80, 1000),
    "net-recover": scenario_recover,
    "net-scaling": scenario_scaling,
    "net-simulcast": scenario_simulcast,
    "bench": scenario_bench,
}


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--guacd", default="127.0.0.1:4823")
    ap.add_argument("scenarios", nargs="+", choices=SCENARIOS)
    a = ap.parse_args()
    failed = 0
    for name in a.scenarios:
        try:
            problems, info = SCENARIOS[name](a.guacd)
        except Exception as e:
            problems, info = [repr(e)], ""
        status = "PASS" if not problems else "FAIL"
        failed += bool(problems)
        print(f"{status} {name}: {info}")
        for p in problems:
            print(f"     - {p}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
