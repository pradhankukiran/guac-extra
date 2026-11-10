# Test target for end-to-end tests: an X desktop with constant motion,
# served over VNC (port 5900, no password) and RDP (port 3389, user "test",
# password "test"), plus a still desktop on VNC port 5901 and full-screen
# video playback (ffmpeg test pattern) on VNC port 5902. For local testing
# only.
FROM ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        xvfb x11vnc xterm openbox xrdp xorgxrdp dbus-x11 procps ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -s /bin/bash test && echo 'test:test' | chpasswd \
    && echo 'exec openbox-session' > /home/test/.xsession \
    && mkdir -p /home/test/.config/openbox \
    && echo '/usr/local/bin/test-desktop-apps' > /home/test/.config/openbox/autostart \
    && chown -R test:test /home/test
COPY docker/test-desktop-start.sh /usr/local/bin/start
COPY docker/test-desktop-apps.sh /usr/local/bin/test-desktop-apps
RUN chmod +x /usr/local/bin/start /usr/local/bin/test-desktop-apps
EXPOSE 5900 5901 5902 3389
CMD ["/usr/local/bin/start"]
