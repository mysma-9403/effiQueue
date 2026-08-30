# Static musl build (rust:alpine defaults to the musl target for its arch).
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app

# Build the dependency graph against a stub first, so editing src/ does not
# recompile every crate. The release profile uses lto + codegen-units=1, which
# makes that rebuild expensive enough to be worth the extra layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
# Cargo skips a rebuild when only mtime changed; force it to see the real source.
RUN touch src/main.rs && cargo build --release --locked

FROM alpine:3.20
RUN adduser -D -H effiqueue
COPY --from=build /app/target/release/effiqueue /usr/local/bin/effiqueue
COPY data/config.slo.toml /etc/effiqueue/config.toml
# NOTE: effiQueue spawns worker processes on THIS host, so the worker runtime
# (e.g. PHP) must be present in the image for real use.
USER effiqueue
ENTRYPOINT ["effiqueue"]
CMD ["--config", "/etc/effiqueue/config.toml"]
