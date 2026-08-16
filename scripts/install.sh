#!/bin/sh
# Install the `areev` CLI from a GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/AreevAI/areev/main/scripts/install.sh | sh
#
# Environment:
#   AREEV_VERSION   tag to install (default: the latest release)
#   AREEV_INSTALL   install directory (default: ~/.local/bin, or /usr/local/bin
#                  when running as root)
#
# POSIX sh on purpose — this has to run in a bare container or a notebook cell.
# Windows is not covered: download the .zip from the Releases page.
set -eu

REPO="AreevAI/areev"
BIN="areev"

die() { printf '%s: %s\n' "$BIN-install" "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# ---- target triple ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) die "unsupported OS '$os' — download a build from https://github.com/$REPO/releases" ;;
esac
case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  aarch64|arm64) arch_part="aarch64" ;;
  *) die "unsupported architecture '$arch' — download a build from https://github.com/$REPO/releases" ;;
esac
target="${arch_part}-${os_part}"

# ---- version ---------------------------------------------------------------
have curl || have wget || die "needs curl or wget"
fetch() {  # fetch <url> <dest|-->
  if have curl; then curl -fsSL "$1" -o "$2"; else wget -qO "$2" "$1"; fi
}

version="${AREEV_VERSION:-}"
if [ -z "$version" ]; then
  # Resolve the latest tag without jq: the redirect target of /releases/latest
  # is the tag URL, and grepping the API body is the fallback.
  if have curl; then
    version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
      "https://github.com/$REPO/releases/latest" 2>/dev/null | sed 's#.*/tag/##')"
  fi
  if [ -z "$version" ] || [ "$version" = "latest" ]; then
    tmp_api="$(mktemp)"
    fetch "https://api.github.com/repos/$REPO/releases/latest" "$tmp_api" \
      || die "could not reach the GitHub API to resolve the latest release"
    version="$(sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' "$tmp_api" | head -n1)"
    rm -f "$tmp_api"
  fi
fi
[ -n "$version" ] || die "could not determine a release to install; set AREEV_VERSION"

plain="${version#v}"
asset="${BIN}-${plain}-${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$version"

# ---- download + verify -----------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'downloading %s (%s)\n' "$asset" "$version" >&2
fetch "$base/$asset" "$tmp/$asset" \
  || die "no prebuilt binary for $target in $version — see https://github.com/$REPO/releases"

# The release carries one SHA256SUMS for every asset. Verify when we can hash;
# say so plainly when we cannot, rather than implying a check that did not run.
if fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
  expected="$(sed -n "s/^\([0-9a-f]\{64\}\)[ *]*$asset\$/\1/p" "$tmp/SHA256SUMS" | head -n1)"
  if [ -n "$expected" ]; then
    if have sha256sum; then actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
    elif have shasum;   then actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
    else actual=""; printf 'warning: no sha256 tool found; skipping checksum\n' >&2
    fi
    if [ -n "$actual" ] && [ "$actual" != "$expected" ]; then
      die "checksum mismatch for $asset (expected $expected, got $actual)"
    fi
  fi
fi

tar xzf "$tmp/$asset" -C "$tmp"

# ---- install ---------------------------------------------------------------
if [ -n "${AREEV_INSTALL:-}" ]; then dest="$AREEV_INSTALL"
elif [ "$(id -u)" = "0" ]; then dest="/usr/local/bin"
else dest="$HOME/.local/bin"
fi
mkdir -p "$dest"
install -m 755 "$tmp/${BIN}-${plain}-${target}/$BIN" "$dest/$BIN" 2>/dev/null \
  || { cp "$tmp/${BIN}-${plain}-${target}/$BIN" "$dest/$BIN" && chmod 755 "$dest/$BIN"; }

printf 'installed %s to %s\n' "$("$dest/$BIN" --version)" "$dest/$BIN" >&2
case ":$PATH:" in
  *":$dest:"*) ;;
  *) printf '\n%s is not on your PATH. Add it:\n  export PATH="%s:$PATH"\n' "$dest" "$dest" >&2 ;;
esac
