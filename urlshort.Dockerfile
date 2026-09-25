FROM rust:1.98.1-bookworm AS builder

ARG SCYLLADB_KNOWN_NODES
ARG REDIS_URL
ARG LISTEN_HOST
ARG PUBLIC_URL

WORKDIR /urlshort

COPY urlshort-server/src ./src
COPY urlshort-server/templates ./templates
COPY urlshort-server/assets ./assets

COPY urlshort-server/Cargo.toml ./
COPY urlshort-server/Cargo.lock ./

RUN cargo build --release

FROM debian:bookworm-slim


WORKDIR /urlshort

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /urlshort/target/release/urlshort-server /urlshort/urlshort-server


EXPOSE 80

CMD ["/urlshort/urlshort-server"]
