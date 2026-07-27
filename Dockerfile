# Static musl build (rust:alpine defaults to the x86_64-unknown-linux-musl target).
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY . .
RUN cargo build --release

FROM alpine:3.20
RUN adduser -D -H effiqueue
COPY --from=build /app/target/release/effiqueue /usr/local/bin/effiqueue
COPY data/config.slo.toml /etc/effiqueue/config.toml
# NOTE: effiQueue spawns worker processes on THIS host, so the worker runtime
# (e.g. PHP) must be present in the image for real use.
USER effiqueue
ENTRYPOINT ["effiqueue"]
CMD ["--config", "/etc/effiqueue/config.toml"]
