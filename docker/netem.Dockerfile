# Network emulation helper for tests: runs tc (netem) inside another
# container's network namespace, e.g.
#   docker run --rm --network container:NAME --cap-add NET_ADMIN \
#       guac-extra/netem:dev tc qdisc show dev eth0
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends iproute2 \
    && rm -rf /var/lib/apt/lists/*
