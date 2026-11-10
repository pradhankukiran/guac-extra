#!/bin/sh
# Moving content for end-to-end tests: a fast-scrolling terminal of
# numbered lines (top) and a clock updating every 50 ms (middle).
xterm -geometry 100x20+0+0 -e seq -f 'line %.0f guac-extra scrolling test' 1 1e12 &
xterm -geometry 60x10+200+400 -e sh -c 'while :; do date +%T.%N; sleep 0.05; done' &
