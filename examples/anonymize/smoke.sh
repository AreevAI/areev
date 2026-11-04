#!/usr/bin/env bash
# Keyless smoke: assert the Indian-identifier fixture set against the built-in
# Tier-0 detectors. Exits non-zero on any miss or false positive.
#
#   examples/anonymize/smoke.sh            # uses `areev` on PATH
#   AREEV=target/debug/areev examples/anonymize/smoke.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
"${AREEV:-areev}" anonymize test --fixtures "$here/india-identifiers.json"
