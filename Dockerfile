FROM rust:1.98.1-bookworm AS builder

ARG DATABASE_URL
ARG REDIS_URL
ARG LISTEN_HOST

ENV SQLX_OFFLINE=true

WORKDIR /urlshort

COPY urlshort-server/src ./src
COPY urlshort-server/migrations ./migrations
COPY urlshort-server/.sqlx ./.sqlx
COPY urlshort-server/Cargo.toml ./
COPY urlshort-server/Cargo.lock ./

RUN cargo build --release

FROM debian:bookworm-slim

WORKDIR /urlshort

COPY --from=builder /urlshort/target/release/urlshort-server /urlshort/urlshort-server

EXPOSE 8001

CMD ["/urlshort/urlshort-server"]
