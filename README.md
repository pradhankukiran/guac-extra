# guac-extra

Apache Guacamole with video streaming.

Stock Guacamole sends the remote screen to the browser as still images
(PNG, JPEG, WebP). That keeps text sharp and costs little when the screen is
still, but scrolling, video and animation drop to low frame rates.

guac-extra keeps everything Guacamole does well (RDP, VNC and SSH, logins,
admin, sharing, recording) and adds a video path taken from
[voxl](https://github.com/tembo/voxl):

- The screen is sent as H.264 video, encoded with x264, in stripes that are
  encoded only when they change.
- Areas that stop moving for 300 ms are sent losslessly (PNG) on a layer
  above the video, so still text is pixel-exact.
- Session recordings include the video, and guacenc renders it.
- The browser decodes the video with WebCodecs, using hardware decoding.
- Video quality follows the measured network speed.

## Status

Version 0.3.0. Browsers with WebCodecs (over HTTPS or localhost) play RDP
and VNC screens as H.264; everyone else gets images. Video needs 2-2.5x
less bandwidth than stock images, and on a 1 Mbit/s link runs at 32 fps
with 111 ms delay where stock images manage 9 fps with 517 ms.

## Layout

| Path | What it holds |
|---|---|
| `video/` | Rust library: the video pipeline from voxl, with a C interface |
| `server/` | apache/guacamole-server, with guac-extra changes |
| `client/` | apache/guacamole-client, with guac-extra changes |
| `docker/` | Dockerfiles and the local dev setup |
| `third_party/licenses/` | Licenses of reused code |

## Build

```sh
make images && make up   # http://localhost:8081/guacamole
```

## Using video

Add to an RDP or VNC connection:

| Parameter | Meaning |
|---|---|
| `enable-video` | `true` to send video to browsers that support it |
| `video-max-kbps` | Bitrate ceiling (default 30000) |
| `video-stripes` | Horizontal stripes, 1-16 (default 1) |
| `video-lossless` | `false` to turn off the lossless overlay for still areas (default on) |
| `video-threads` | Most encoder threads per session, 1-16 (default 2) |
| `video-scaling` | `false` to keep full resolution on links slower than 500 kbit/s (default on) |
| `video-simulcast` | `false` to keep one stream even when one user's link is much slower (default on) |
| `video-passthrough` | RDP: `true` to forward Windows' own H.264 instead of re-encoding (experimental, default off) |

## License

Apache License 2.0. Includes code derived from voxl (MIT). See
[NOTICE](NOTICE).
