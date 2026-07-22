FROM rust:1.97-bookworm AS chef
RUN cargo install cargo-chef --version 0.1.77 --locked
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home vault
WORKDIR /app
COPY --from=builder /build/target/release/clipboard-vault /usr/local/bin/clipboard-vault
COPY templates ./templates
COPY static ./static
COPY migrations ./migrations
RUN mkdir -p /data/uploads && chown -R vault:vault /data
USER vault
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --start-interval=1s --retries=3 CMD curl -fsS http://127.0.0.1:8080/health/live || exit 1
ENTRYPOINT ["/usr/local/bin/clipboard-vault"]
