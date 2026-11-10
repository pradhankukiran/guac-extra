#!/bin/sh
# VNC: a 1024x768 Xvfb desktop with two moving xterms.
Xvfb :1 -screen 0 1024x768x24 -nolisten tcp &
sleep 1
DISPLAY=:1 openbox &
DISPLAY=:1 /usr/local/bin/test-desktop-apps
x11vnc -display :1 -forever -shared -nopw -quiet -rfbport 5900 &

# VNC: a still 1024x768 desktop (openbox only) on port 5901; tests change it
# with a right-click (openbox's root menu).
Xvfb :2 -screen 0 1024x768x24 -nolisten tcp &
sleep 1
DISPLAY=:2 openbox &
x11vnc -display :2 -forever -shared -nopw -quiet -rfbport 5901 &

# VNC: full-screen video playback (a moving 30 fps test pattern) on 5902,
# the content video streaming helps most with.
Xvfb :3 -screen 0 1024x768x24 -nolisten tcp &
sleep 1
DISPLAY=:3 SDL_AUDIODRIVER=dummy ffplay -loglevel quiet -an -noborder -left 0 -top 0 \
    -x 1024 -y 768 -f lavfi -i testsrc2=size=1024x768:rate=30 &
x11vnc -display :3 -forever -shared -nopw -quiet -rfbport 5902 &

# RDP: xrdp starts its own Xorg session per login (user test / test).
mkdir -p /var/run/xrdp
xrdp-sesman
exec xrdp --nodaemon
