# syntax=docker/dockerfile:1
# guacd for guac-extra: guacamole-server (server/) linked with the Rust
# video library (video/). Build from the repo root:
#   docker build -f docker/guacd.Dockerfile -t guac-extra/guacd .

ARG UBUNTU=24.04

# ---- video library ----------------------------------------------------------
# Built on the same Ubuntu as guacd so x264's headers match the library guacd
# links at runtime.
FROM ubuntu:${UBUNTU} AS video
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config curl ca-certificates \
        libx264-dev libyuv-dev libclang-dev ffmpeg \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
WORKDIR /build/video
COPY video/ ./
RUN --mount=type=cache,target=/opt/cargo/registry \
    --mount=type=cache,target=/build/video/target-cache \
    CARGO_TARGET_DIR=/build/video/target-cache cargo test --release \
    && CARGO_TARGET_DIR=/build/video/target-cache cargo build --release \
    && mkdir -p target/release \
    && cp target-cache/release/libguac_extra_video.a target/release/
# C API smoke test: build against the header and static library, run it,
# and check ffprobe decodes every frame it wrote.
RUN cc -std=c99 -Wall -Wextra -Werror -pedantic -Iinclude tests/c/smoke.c \
        target/release/libguac_extra_video.a -lx264 -lyuv -lpthread -ldl -lm -lrt -lutil \
        -o /tmp/smoke \
    && /tmp/smoke /tmp/smoke.h264 \
    && test "$(ffprobe -v error -count_frames -select_streams v:0 \
               -show_entries stream=nb_read_frames -of csv=p=0 /tmp/smoke.h264)" = 20

# ---- guacamole-server -------------------------------------------------------
FROM ubuntu:${UBUNTU} AS server
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential autoconf automake libtool pkg-config \
        libcairo2-dev libjpeg-turbo8-dev libpng-dev uuid-dev \
        freerdp3-dev libwebp-dev libpulse-dev libvorbis-dev \
        libssh2-1-dev libssl-dev libtelnet-dev libvncserver-dev \
        libwebsockets-dev libavcodec-dev libavformat-dev libavutil-dev \
        libswscale-dev libcunit1-dev libpango1.0-dev libx264-dev libyuv-dev \
    && rm -rf /var/lib/apt/lists/*

COPY --from=video /build/video/include /build/video/include
COPY --from=video /build/video/target/release/libguac_extra_video.a \
                  /build/video/target/release/libguac_extra_video.a
COPY server/ /build/server/
WORKDIR /build/server
RUN autoreconf -fi \
    && ./configure --prefix=/opt/guacamole --with-guac-extra-video=yes \
           CFLAGS="-O2 -Wno-error=deprecated-declarations" \
    && make -j"$(nproc)" \
    && make check \
    && make install

# Record which Ubuntu packages the installed binaries need at runtime
RUN FREERDP_PLUGINS="$(find /usr/lib -type d -name 'freerdp3' | head -1)" \
    && echo "$FREERDP_PLUGINS" > /opt/guacamole/freerdp-plugin-dir \
    && find /opt/guacamole "$FREERDP_PLUGINS" -type f \( -name '*.so*' -o -path '*/sbin/*' -o -path '*/bin/*' \) \
         -exec ldd {} + 2>/dev/null \
       | awk '/=> \// {print $3}' | sort -u | xargs -r readlink -f \
       | xargs -r dpkg -S 2>/dev/null | cut -d: -f1 | sort -u \
       > /opt/guacamole/runtime-packages \
    && mkdir -p /opt/freerdp-plugins && cp -a "$FREERDP_PLUGINS"/. /opt/freerdp-plugins/

# ---- runtime ----------------------------------------------------------------
FROM ubuntu:${UBUNTU}
ARG DEBIAN_FRONTEND=noninteractive
COPY --from=server /opt/guacamole /opt/guacamole
COPY --from=server /opt/freerdp-plugins /opt/freerdp-plugins
RUN apt-get update \
    && xargs -a /opt/guacamole/runtime-packages apt-get install -y --no-install-recommends \
    && apt-get install -y --no-install-recommends ca-certificates fonts-dejavu-core \
         netcat-openbsd \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p "$(cat /opt/guacamole/freerdp-plugin-dir)" \
    && cp -an /opt/freerdp-plugins/. "$(cat /opt/guacamole/freerdp-plugin-dir)/" \
    && rm -rf /opt/freerdp-plugins \
    && echo /opt/guacamole/lib > /etc/ld.so.conf.d/guacamole.conf && ldconfig \
    && useradd --system --create-home --shell /usr/sbin/nologin guacd

USER guacd
EXPOSE 4822
HEALTHCHECK --interval=30s --timeout=5s CMD nc -z 127.0.0.1 4822 || exit 1
CMD ["/opt/guacamole/sbin/guacd", "-b", "0.0.0.0", "-L", "info", "-f"]
