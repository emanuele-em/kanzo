FROM rust:1-alpine AS build
WORKDIR /src
COPY . .
# Cache mounts keep crates and build output between builds; the binary is copied out of the cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/kanzo /kanzo

# Alpine for busybox: `hostname` lets the board accept the server's name, `wget` runs the healthcheck.
# Git for sync: compose gives it the repo's credentials.
FROM alpine:3
RUN apk add --no-cache git
COPY --from=build /kanzo /usr/local/bin/kanzo
ENV KANZO_HOME=/data
# Not root, also under a plain `docker run`; compose sets its own user.
USER 1000:1000
ENTRYPOINT ["kanzo"]
# The port is fixed here so a `port` edit in config.toml cannot break the published mapping.
CMD ["--listen", "0.0.0.0", "--port", "4242", "--no-open"]
