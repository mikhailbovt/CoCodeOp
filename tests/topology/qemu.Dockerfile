FROM debian:trixie-slim@sha256:7378260c0a098d27ac206225db5e7d92b8c7c84ea7afccc86eeb513756bbd392

RUN apt-get update \
    && apt-get install -y --no-install-recommends cloud-image-utils qemu-system-x86 qemu-utils socat \
    && rm -rf /var/lib/apt/lists/*

ENTRYPOINT ["qemu-system-x86_64"]
