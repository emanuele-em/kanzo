# syntax=docker/dockerfile:1
FROM docker.io/library/rust:alpine AS builder

RUN apk add --no-cache musl-dev gcc

WORKDIR /usr/src/kanzo

COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY CHANGELOG.md ./

RUN cargo build --release

FROM docker.io/library/alpine:3.21

RUN apk add --no-cache ca-certificates git tzdata

RUN addgroup -g 1000 kanzo && \
    adduser -u 1000 -G kanzo -s /bin/sh -D kanzo

RUN mkdir -p /data && chown -R kanzo:kanzo /data

USER kanzo:kanzo
WORKDIR /data

ENV KANZO_HOME=/data \
    KANZO_LISTEN=0.0.0.0

EXPOSE 4242

COPY --from=builder /usr/src/kanzo/target/release/kanzo /usr/local/bin/kanzo

VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -qO- http://127.0.0.1:4242/api/ping || exit 1

ENTRYPOINT ["kanzo"]
CMD ["--no-open"]
