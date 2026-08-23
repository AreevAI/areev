# The areev container image: the one `areev` binary, built with the two
# non-default features a container deployment wants — `postgres` (the server
# tier, --db postgres://…?schema=<name>) and `tls` (native rustls for
# deployments with nowhere to run a terminating proxy).
#
#   docker build -t areev .
#
# Build as `areev:latest` deliberately: that is the image name
# `areev trigger render --target k8s-cronjob` emits. One image, two roles —
# `ui` (console) and `heartbeat` (an image-provided loop of one-shot
# `areev trigger run`; see docker/heartbeat.sh). Everything else is the CLI
# verbatim. Guide: docs/docker.md

FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts keep the registry and build artifacts across rebuilds; the
# binary is copied out because the target dir does not survive the RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p areev --bin areev --features postgres,tls \
    && cp target/release/areev /usr/local/bin/areev

FROM debian:bookworm-slim
# Nothing to apt-install: TLS is rustls with compiled-in webpki roots, and
# turso's C pieces are statically linked — the runtime needs glibc and /bin/sh
# (for --tool-cmd / --connector-cmd subprocesses), both already here.
COPY --from=build /usr/local/bin/areev /usr/local/bin/areev
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY --chmod=755 docker/heartbeat.sh  /usr/local/bin/areev-heartbeat

RUN useradd --uid 10001 --create-home areev \
    && mkdir -p /data && chown areev:areev /data
USER areev
WORKDIR /data
VOLUME /data

# Every verb falls back to $AREEV_DB, so `docker run … areev add john …`
# needs no --db flag. Override per container (a path, or a postgres:// DSN).
ENV AREEV_DB=/data/areev.db

EXPOSE 7437

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["--help"]
