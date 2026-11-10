# guac-extra

**Apache Guacamole with H.264 video streaming.** Smooth, low-bandwidth
remote desktops in the browser, with nothing to install on the client.

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
![Status: pre-1.0](https://img.shields.io/badge/status-pre--1.0-orange.svg)
![Protocols: RDP | VNC](https://img.shields.io/badge/video-RDP%20%7C%20VNC-informational.svg)

Stock Guacamole sends the remote screen as still images (PNG, JPEG, WebP).
That keeps text sharp, but scrolling, video playback and animation become
choppy, and slow links fall far behind. guac-extra keeps everything
Guacamole does well (RDP, VNC and SSH, authentication, administration,
session sharing, recording) and adds a real video path built on the
pipeline of [voxl](https://github.com/tembo/voxl):

- **H.264 video** for moving content, decoded by the browser's hardware
  through WebCodecs.
- **Pixel-exact still content:** areas that stop moving for 300 ms are sent
  as PNG on a layer above the video.
- **Adaptive by design:** bitrate follows each viewer's measured link,
  drops to half resolution on very slow links, and moves a slow viewer to
  its own stream so it doesn't hold others back.
- **Drop-in:** same protocol, same web app, same configuration. Browsers
  that can't play the video get stock images, in the same session.

## Contents

- [Results](#results)
- [How it works](#how-it-works)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Operations](#operations)
- [Development](#development)
- [Project status](#project-status)
- [Acknowledgements](#acknowledgements)
- [License](#license)

## Results

Measured against stock Guacamole on the project's test desktop (1024x768,
VNC), with guacd's traffic shaped by netem. Frame rate is frames
delivered; delay is the one-way delay of frames (p95).

| Link | guac-extra | Stock Guacamole |
|---|---|---|
| 10 Mbit/s, 40 ms | 34 fps, 1.6 Mbit/s, 42 ms | 34 fps, 3.3 Mbit/s, 55 ms |
| 2 Mbit/s, 60 ms | 34 fps, 70 ms | 18 fps, 165 ms |
| 1 Mbit/s, 80 ms | 32 fps, 121 ms | 8 fps, 825 ms |

- Video needs 2 to 2.5 times less bandwidth than images on the same content.
- On a 300 kbit/s link, dropping to half resolution keeps video at 34 fps
  and 126 ms, against 11 fps and 350 ms at full resolution.
- guacd CPU per session: equal to stock on text, about 40% lower on video
  playback.
- With one viewer on a 300 kbit/s link, another viewer in the same session
  keeps full resolution and about 1 Mbit/s instead of dropping to
  129 kbit/s.

These are lab numbers for one kind of content. Real content mixes, Windows
RDP servers and large numbers of sessions are not measured yet.

## How it works

```
                                ┌─ moving areas ─► H.264 (x264, stripes) ─► video layers ──┐
RDP / VNC ─► guacd ─► changes ──┤                                                           ├─► browser
                                └─ still areas ──► PNG (after 300 ms) ───► overlay layer ──┘   WebCodecs + canvas
```

- **guacd** (C, from apache/guacamole-server) detects changes as usual and
  hands the main screen's changes to the video library instead of the
  image encoder. Users whose browsers can't decode the video keep getting
  images.
- **The video library** (Rust, `video/`) is voxl's pipeline behind a C API:
  libyuv color conversion, x264 tuned for desktop content, horizontal
  stripes encoded in parallel and only when they change, ack-based flow
  control and adaptive bitrate.
- **The browser player** (`Guacamole.H264VideoPlayer` in
  guacamole-common-js) decodes with WebCodecs, draws each frame on arrival,
  and acknowledges every decoded frame so guacd can pace the stream.
- **The protocol is unchanged:** video travels in Guacamole's existing
  `video` and `blob` instructions, with a small header per frame.

## Quick start

Requirements: Docker, GNU make, and a browser with WebCodecs (current
Chrome, Edge, Firefox or Safari). Browsers enable WebCodecs only on HTTPS or
`http://localhost`.

```sh
git clone https://github.com/pradhankukiran/guac-extra.git
cd guac-extra
make images         # builds guac-extra/guacd:dev and guac-extra/web:dev
make test-desktop   # builds the test desktop (VNC and RDP) used by the dev setup
make up             # starts guacd, the web app and the test desktop
```

Open <http://localhost:8081/guacamole> and log in with the user defined in
`docker/dev/guac-home/user-mapping.xml`. The "Test desktop" connections
compare video with stock images side by side.

### Using the images in an existing deployment

The images replace the official ones and take the same settings:

| Official image | guac-extra image |
|---|---|
| `guacamole/guacd` | `guac-extra/guacd` |
| `guacamole/guacamole` | `guac-extra/web` |

Replace both: the encoder lives in guacd, the player in the web app.
Existing connections, databases and extensions keep working; video stays
off until enabled per connection.

## Configuration

### Connection parameters

Set on RDP or VNC connections (in the admin interface, under
"Video (guac-extra)"):

| Parameter | Default | Effect |
|---|---|---|
| `enable-video` | off | Send the screen as H.264 to browsers that support it |
| `video-max-kbps` | 30000 | Bitrate ceiling for adaptive bitrate |
| `video-stripes` | 1 | Horizontal stripes (1-16), encoded in parallel |
| `video-threads` | 2 | Most encoder threads per session (1-16) |
| `video-lossless` | on | `false` turns off the lossless overlay for still areas |
| `video-scaling` | on | `false` keeps full resolution on links under 500 kbit/s |
| `video-simulcast` | on | `false` keeps one stream even when one viewer is much slower |
| `video-focus` | on | `false` turns off the sharper area around the mouse pointer |
| `video-passthrough` | off | RDP only: forward the server's own H.264 instead of re-encoding (experimental) |

### Bandwidth policies

Cap video per user or group in `guacamole.properties`:

```
video-bandwidth-policy: group:students=2000, group:staff=10000, user:alice=5000, default=4000
```

A user's own rule wins, then the highest rule among their groups, then
`default`. Every connection receives the result as the parameter token
`${GUAC_EXTRA_MAX_KBPS}`; set a connection's `video-max-kbps` to that token
and each user gets their own cap.

### When video is not used

guacd logs the reason each time:

- The user's browser has no WebCodecs, isn't on HTTPS or localhost, or
  can't decode H.264: that user gets images, others keep video.
- guacd was built without the video library.

## Operations

### Metrics

Set `GUACD_METRICS_PORT` on guacd to serve Prometheus metrics at
`/metrics` and a health check at `/health`:

```
guac_extra_video_active{connection="$..."} 1
guac_extra_video_users{connection="$...",kind="video"} 2
guac_extra_video_frames_total{connection="$..."} 18342
guac_extra_video_target_kbps{connection="$..."} 4200
```

Also exported: users by kind (`video`, `video_low`, `paused`, `images`),
keyframes, bytes, resolution scale, passthrough, overlay images, fallbacks
and errors. The endpoint has no authentication; publish it only to your
monitoring network.

### Admin dashboard

Administrators get a **Video** tab under Settings with every session's
mode, users, frame rate, bitrate, keyframes and fallbacks, refreshed every
two seconds. It reads guacd's metrics endpoint
(`guacd-metrics-url`, default `http://<guacd-hostname>:9100/metrics`).

### Statistics in the browser

Any user can turn on **Show video statistics** in the Guacamole menu
(Ctrl+Alt+Shift) to see frame rate, bitrate, decode time, smoothing delay
and dropped frames for their session.

### Recordings

Session recordings include the video. `guacenc` in the guacd image decodes
it, and the browser recording player uses the same video player.

## Development

```
server/   apache/guacamole-server with guac-extra changes (C)
client/   apache/guacamole-client with guac-extra changes (Java, JavaScript)
video/    the video library: voxl's pipeline with a C API (Rust)
docker/   Dockerfiles, dev setup, test desktop, network emulation
tests/    end-to-end tests that speak the Guacamole protocol to guacd
```

Everything builds in Docker; the host needs only Docker, make and, for the
end-to-end tests, Python 3 and ffprobe.

| Command | What it does |
|---|---|
| `make images` | Build the guacd and web app images (runs their unit tests) |
| `make video-test` | Video library unit and pipeline tests, plus a C API test |
| `make e2e` | End-to-end scenarios against guacd: video, mixed users, stripes, overlay, recording, resize, hidden tabs, metrics |
| `make e2e-net` | Video vs stock images on shaped links, recovery, scaling, simulcast |
| `make bench` | Frame rate, bandwidth, delay and CPU on a terminal and on video playback |
| `make images-arm64` | ARM64 images (built under emulation) |

The end-to-end tests check every H.264 stream with ffprobe, compare the
lossless overlay pixel for pixel with the desktop, and render recordings
with guacenc. Upstream sources and the commits they were taken from are
listed in [UPSTREAM.md](UPSTREAM.md).

## Project status

Pre-1.0. The core video path is tested end to end: video, mixed users,
stripes, lossless overlay, recordings, resizing, hidden tabs, adaptive
bitrate, dynamic resolution and simulcast.

Known gaps:

- **Experimental:** `video-passthrough` has not been tested against an RDP
  server that sends H.264.
- **Built but not yet tested end to end:** the admin dashboard, bandwidth
  policies, the focus area and playback smoothing.
- **ARM64:** images build and pass unit tests under emulation; connections
  have not been checked on ARM64 hardware.
- **Not supported:** GPU encoding, AV1/VP9, WebRTC transport.
- No hosted CI yet; tests run locally with the make targets above.

## Acknowledgements

- [Apache Guacamole](https://guacamole.apache.org/), the foundation this
  project builds on.
- [voxl](https://github.com/tembo/voxl) by Tembo, whose video pipeline
  (encoding, stripes, paint-over, flow control and bitrate control) the
  video library is derived from.
- [x264](https://www.videolan.org/developers/x264.html),
  [libyuv](https://chromium.googlesource.com/libyuv/libyuv/) and
  [FreeRDP](https://www.freerdp.com/).

guac-extra is an independent project. It is not affiliated with or endorsed
by the Apache Software Foundation. Apache Guacamole and Apache are
trademarks of the Apache Software Foundation.

## License

[Apache License 2.0](LICENSE). Includes code derived from voxl under the
MIT License ([third_party/licenses/voxl-MIT.txt](third_party/licenses/voxl-MIT.txt));
see [NOTICE](NOTICE).
