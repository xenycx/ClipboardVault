# Clipboard Vault 3.x: one static binary, one /data volume.
#
# The builder is Alpine, so Rust targets musl and the binary is statically linked
# (SQLite is compiled in). The runtime is distroless/static: no shell, no package
# manager, no libc. `docker buildx build --platform linux/amd64,linux/arm64`
# produces both architectures from this file.

FROM rust:1.97-alpine AS chef
RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --version 0.1.77 --locked
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --bin clipboard-vault --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
RUN cargo build --locked --release --bin clipboard-vault \
    && mkdir -p /out/data/uploads/tmp /out/data/backups \
    && cp target/release/clipboard-vault /out/clipboard-vault

FROM gcr.io/distroless/static-debian12:nonroot AS runtime
# Templates and migrations are compiled into the binary. Static assets are served
# from ./static (relative to /app) and fingerprinted at startup.
WORKDIR /app
COPY --from=builder /out/clipboard-vault /usr/local/bin/clipboard-vault
COPY static ./static
COPY --from=builder --chown=10001:10001 /out/data /data
USER 10001:10001
ENV DATA_DIR=/data \
    PORT=8080
EXPOSE 8080
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --start-interval=1s --retries=3 \
    CMD ["/usr/local/bin/clipboard-vault", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/clipboard-vault"]
