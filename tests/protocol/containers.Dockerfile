FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS rust-base

FROM rust-base AS probe-server
WORKDIR /build
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY apps/ apps/
COPY crates/ crates/
COPY migrations/ migrations/
COPY tests/protocol/ tests/protocol/
RUN cargo build --locked --release -p office-compat-probe \
    && install target/release/office-compat-probe /usr/local/bin/office-compat-probe \
    && rm -rf /build/target
ENTRYPOINT ["office-compat-probe"]

FROM node:24.15.0-bookworm-slim@sha256:4e6b70dd6cbfc88c8157ba19aa3d9f9cce6ba4703576d55459e45efcbc9c5f5d AS probe-clients
COPY --from=rust-base /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
WORKDIR /opt/probe
COPY tests/protocol/clients/package.json tests/protocol/clients/package-lock.json ./
RUN npm ci --ignore-scripts --no-audit --no-fund
ENV PATH=/opt/probe/node_modules/.bin:$PATH
COPY tests/protocol/clients/run.mjs ./
USER 1000:1000
ENTRYPOINT ["node", "/opt/probe/run.mjs"]
