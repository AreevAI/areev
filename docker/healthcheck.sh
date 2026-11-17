#!/bin/bash
# HEALTHCHECK for the areev image. bash, not sh: the probe is bash's built-in
# /dev/tcp, so the image needs no curl or wget (debian:bookworm-slim ships
# neither, and bash is already there).
#
# One image, several roles (docs/docker.md). Only `ui` serves HTTP, so:
#   - a container running `areev ui …` is probed at GET /healthz on the port
#     its own --addr names (default 7437) — the route answers before auth,
#     so no token is needed even under --token-env;
#   - any other role (heartbeat, a one-shot verb) has no endpoint to probe
#     and reports healthy: a heartbeat's liveness is its process, which
#     Docker already watches via the restart policy.
#
# Under --tls-cert the console speaks only TLS, which bash cannot; the probe
# then degrades to "the port accepts connections".
set -u

ui_args=()
for f in /proc/[0-9]*/cmdline; do
  [ -r "$f" ] || continue
  mapfile -d '' -t argv < "$f" 2>/dev/null || continue
  [ "${#argv[@]}" -ge 2 ] || continue
  case "${argv[0]}" in areev|*/areev) ;; *) continue ;; esac
  [ "${argv[1]}" = "ui" ] && { ui_args=("${argv[@]}"); break; }
done
[ "${#ui_args[@]}" -gt 0 ] || exit 0

addr="127.0.0.1:7437"
tls=""
for ((i = 0; i < ${#ui_args[@]}; i++)); do
  case "${ui_args[$i]}" in
    --addr) addr="${ui_args[$((i + 1))]:-$addr}" ;;
    --tls-cert) tls=1 ;;
  esac
done
port="${addr##*:}"
host="${addr%:*}"
host="${host#[}"
host="${host%]}"
case "$host" in "" | 0.0.0.0 | ::) host=127.0.0.1 ;; esac

exec 3<>"/dev/tcp/$host/$port" || exit 1
[ -n "$tls" ] && exit 0
printf 'GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n' >&3
IFS= read -r -t 5 status <&3 || exit 1
case "$status" in
  "HTTP/1.1 200"*) exit 0 ;;
  *) exit 1 ;;
esac
