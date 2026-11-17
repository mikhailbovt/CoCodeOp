# syntax=docker/dockerfile:1
# Build context contains the Linux office-runner binary from office-server.Dockerfile's runner-binary target.
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc AS base

FROM base AS codex-package
ADD --checksum=sha256:52f1b8f5cf66fe776d2bd5e68a32ef6884b7e466fd80614bf8858046dfa9012f https://registry.npmjs.org/@openai/codex/-/codex-0.148.0-linux-x64.tgz /tmp/codex.tgz
RUN mkdir -p /opt/cocodeop/codex \
    && tar -xf /tmp/codex.tgz -C /opt/cocodeop/codex --strip-components=3 package/vendor/x86_64-unknown-linux-musl

FROM base AS codex
COPY --from=codex-package /opt/cocodeop/codex /opt/cocodeop/codex
COPY --chmod=755 office-runner /usr/local/bin/office-runner
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/office-runner", "worker-agent", "--backend", "codex"]

FROM base AS opencode-package
ADD --checksum=sha256:2249989386aab6b4055d0e8b79fa09874e715c3b15805728543211514b872d88 https://registry.npmjs.org/opencode-linux-x64-baseline/-/opencode-linux-x64-baseline-1.19.5.tgz /tmp/opencode.tgz
RUN mkdir -p /opt/cocodeop \
    && tar -xf /tmp/opencode.tgz -C /opt/cocodeop --strip-components=2 package/bin/opencode

FROM base AS opencode
COPY --from=opencode-package /opt/cocodeop/opencode /opt/cocodeop/opencode
COPY --chmod=755 office-runner /usr/local/bin/office-runner
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/office-runner", "worker-agent", "--backend", "opencode"]
