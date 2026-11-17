# syntax=docker/dockerfile:1
FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build
# The pinned base already contains the compiler. Release builds need no rustup downloads.
ENV RUSTUP_TOOLCHAIN=1.97.1
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps/ apps/
COPY crates/ crates/
COPY migrations/ migrations/
COPY tests/protocol/Cargo.toml tests/protocol/Cargo.toml
COPY tests/protocol/src/ tests/protocol/src/
# Cargo's cache can be newer than the source timestamps in a different checkout.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target,sharing=locked \
    find apps crates migrations tests -type f -exec touch {} + \
    && touch Cargo.toml Cargo.lock rust-toolchain.toml \
    && cargo build --locked --release -p office-server -p office-cli -p office-runner \
    && install target/release/office-server target/release/office target/release/office-runner /usr/local/bin/

FROM scratch AS runner-binary
COPY --from=build /usr/local/bin/office-runner /office-runner

FROM scratch AS binaries
COPY --from=build /usr/local/bin/office-server /usr/local/bin/office /usr/local/bin/office-runner /

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /usr/local/bin/office-server /usr/local/bin/office /usr/local/bin/
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/office-server"]
