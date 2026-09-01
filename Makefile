# guac-extra top-level build. Everything heavy runs in Docker, so the host
# only needs Docker and (for `make video`) Rust.

.PHONY: all video test video-test images guacd web up down logs clean test-desktop e2e netem e2e-net bench release guacd-arm64 web-arm64 images-arm64

all: images

## Rust video library on the host (fast feedback while developing)
video:
	cd video && cargo build --release

test:
	cd video && cargo test --release

## Video library tests on Ubuntu 24.04 in Docker (no host packages needed)
video-test:
	docker build --target video -f docker/guacd.Dockerfile -t guac-extra/video-build:dev .

## Docker images
images: guacd web

guacd:
	docker build -f docker/guacd.Dockerfile -t guac-extra/guacd:dev .

web:
	docker build -t guac-extra/web:dev client/

## End-to-end video tests against guacd (needs python3 and ffprobe)
test-desktop:
	docker build -f docker/test-desktop.Dockerfile -t guac-extra/test-desktop:dev .

e2e: guacd test-desktop
	docker compose -f docker/e2e.compose.yaml up -d --force-recreate
	sleep 5
	python3 -I tests/e2e/guac_video_client.py vnc-video vnc-images vnc-mixed vnc-mixed-badtype \
		vnc-stripes vnc-paintover vnc-keyframe vnc-badtype rdp-video \
		vnc-shared vnc-recording rdp-resize vnc-lossless vnc-hidden metrics; \
		status=$$?; docker compose -f docker/e2e.compose.yaml down; exit $$status

## Video vs images on shaped links (10, 5, 2, 1 Mbit/s) and bitrate recovery
netem:
	docker build -f docker/netem.Dockerfile -t guac-extra/netem:dev .

e2e-net: guacd test-desktop netem
	docker compose -f docker/e2e.compose.yaml up -d --force-recreate
	sleep 5
	python3 -I tests/e2e/guac_video_client.py net-10mbit net-5mbit net-2mbit \
		net-1mbit net-recover net-scaling net-simulcast; \
		status=$$?; docker compose -f docker/e2e.compose.yaml down; exit $$status

## Video vs images on a terminal and on video playback (prints a table)
bench: guacd test-desktop
	docker compose -f docker/e2e.compose.yaml up -d --force-recreate
	sleep 5
	python3 -I tests/e2e/guac_video_client.py bench; \
		status=$$?; docker compose -f docker/e2e.compose.yaml down; exit $$status

## Tagged images: make release VERSION=0.1.0 (local only; no push)
release: images
	@test -n "$(VERSION)" || (echo "usage: make release VERSION=x.y.z" && exit 1)
	docker tag guac-extra/guacd:dev guac-extra/guacd:$(VERSION)
	docker tag guac-extra/web:dev guac-extra/web:$(VERSION)
	@echo "Tagged guac-extra/guacd:$(VERSION) and guac-extra/web:$(VERSION)"

## ARM64 images (built under emulation on x86; guacd is slow to build that way)
guacd-arm64:
	docker buildx build --platform linux/arm64 -f docker/guacd.Dockerfile \
		-t guac-extra/guacd:dev-arm64 --load .

web-arm64:
	docker buildx build --platform linux/arm64 -t guac-extra/web:dev-arm64 --load client/

images-arm64: guacd-arm64 web-arm64

## Local dev environment on http://localhost:8081/guacamole
up:
	docker compose -f docker/compose.yaml up -d

down:
	docker compose -f docker/compose.yaml down

logs:
	docker compose -f docker/compose.yaml logs -f

clean:
	cd video && cargo clean
