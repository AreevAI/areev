#!/usr/bin/env python3
"""Inspect the built blobs and write `dist/blessed.json`.

A blessed tool's identity is its content address, so this is the file that
records it — the addresses `docs/blessed-tools.md` quotes, `pack install`
pins, and `--allow-executor` names. It also asserts the two properties the
sandbox will check anyway, here, where the failure is a build error rather
than a refused run at 3am:

  * the import set is exactly what the tool's capability declaration admits
    (an unused-but-imported `areev::fetch` is refused at instantiation), and
  * the declared memory maximum is at or below the sandbox's page ceiling.
"""
import hashlib
import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
DIST = HERE / "dist"

# tool name -> (crate artifact, runtime, permitted imports, one-line summary)
TOOLS = {
    "http.call": (
        "areev_tool_http_call.wasm",
        "wasm32-areev-io",
        {"areev::emit", "areev::fetch"},
        "One brokered HTTP request, forwarded verbatim. The declaration is the policy.",
    ),
    "mcp.call": (
        "areev_tool_mcp_call.wasm",
        "wasm32-areev-io",
        {"areev::emit", "areev::fetch"},
        "Call a tool on an MCP server (JSON-RPC 2.0 `tools/call`) and unwrap `result`.",
    ),
    "a2a.call": (
        "areev_tool_a2a_call.wasm",
        "wasm32-areev-io",
        {"areev::emit", "areev::fetch"},
        "Send a message to an A2A agent (JSON-RPC 2.0 `message/send`) and unwrap `result`.",
    ),
    "mailbox.poll": (
        "areev_tool_mailbox_poll.wasm",
        "wasm32-areev-io",
        {"areev::emit", "areev::blob_get"},
        "The example trigger connector: reads a filed feed by content address (#185).",
    ),
}

# The sandbox's default ceiling (areev-sandbox `DEFAULT_MAX_PAGES`).
MAX_PAGES = 256


def _leb(b, i):
    r = s = 0
    while True:
        x = b[i]
        i += 1
        r |= (x & 0x7F) << s
        s += 7
        if not x & 0x80:
            return r, i


def _name(b, i):
    n, i = _leb(b, i)
    return b[i : i + n].decode(), i + n


def inspect(path):
    """(imports, exports, (min_pages, max_pages)) of a wasm module."""
    b = path.read_bytes()
    if b[:4] != b"\0asm":
        raise SystemExit(f"{path}: not a wasm module")
    i = 8
    imports, exports, mem = [], [], (0, None)
    while i < len(b):
        sid = b[i]
        i += 1
        size, i = _leb(b, i)
        end = i + size
        if sid == 2:  # imports
            cnt, j = _leb(b, i)
            for _ in range(cnt):
                m, j = _name(b, j)
                n, j = _name(b, j)
                kind = b[j]
                j += 1
                if kind == 0:
                    _, j = _leb(b, j)
                elif kind == 1:
                    j += 1
                    flags = b[j]
                    j += 1
                    _, j = _leb(b, j)
                    if flags & 1:
                        _, j = _leb(b, j)
                elif kind == 2:
                    flags = b[j]
                    j += 1
                    _, j = _leb(b, j)
                    if flags & 1:
                        _, j = _leb(b, j)
                elif kind == 3:
                    j += 2
                imports.append(f"{m}::{n}")
        elif sid == 5:  # memory
            cnt, j = _leb(b, i)
            for _ in range(cnt):
                flags = b[j]
                j += 1
                lo, j = _leb(b, j)
                hi = None
                if flags & 1:
                    hi, j = _leb(b, j)
                mem = (lo, hi)
        elif sid == 7:  # exports
            cnt, j = _leb(b, i)
            for _ in range(cnt):
                n, j = _name(b, j)
                j += 1
                _, j = _leb(b, j)
                exports.append(n)
        i = end
    return imports, exports, mem


def main():
    # `--check` verifies the COMMITTED blobs rather than rebuilding: a rebuild
    # on a different rustc produces different bytes, and a gate that demanded
    # byte-identical output from every toolchain would fail honestly-unchanged
    # trees. What must hold on every machine is that the committed blob still
    # hashes to the address the manifest publishes, still imports only what its
    # capability admits, and still fits the page ceiling.
    check = "--check" in sys.argv
    target = HERE / "target/wasm32-unknown-unknown/release"
    DIST.mkdir(exist_ok=True)
    out = {
        "note": (
            "Blessed wasm32-areev-io tools (#179). A tool's identity is its content "
            "address: pin it with --allow-executor and name it from a Definition's "
            "executor_uri. Rebuild with areev-tools/build.sh; docs/blessed-tools.md "
            "documents the contracts."
        ),
        "tools": {},
    }
    failures = []
    for tool, (artifact, runtime, permitted, summary) in TOOLS.items():
        blob = DIST / f"{tool}.wasm"
        if not check:
            src = target / artifact
            if not src.exists():
                failures.append(f"{tool}: {src} is missing — run build.sh")
                continue
            blob.write_bytes(src.read_bytes())
        if not blob.exists():
            failures.append(f"{tool}: {blob} is missing — run areev-tools/build.sh")
            continue
        imports, exports, (lo, hi) = inspect(blob)
        got = set(imports)
        if got != permitted:
            failures.append(
                f"{tool}: imports {sorted(got)} but its declaration admits "
                f"{sorted(permitted)} — the sandbox refuses an unlinked import by name"
            )
        for needed in ("alloc", "run", "memory"):
            if needed not in exports:
                failures.append(f"{tool}: exports no `{needed}`")
        if hi is None or hi > MAX_PAGES:
            failures.append(
                f"{tool}: declares max {hi} memory pages, above the {MAX_PAGES} ceiling — "
                f"see .cargo/config.toml"
            )
        digest = hashlib.sha256(blob.read_bytes()).hexdigest()
        out["tools"][tool] = {
            "summary": summary,
            "address": f"cas://sha256:{digest}",
            "sha256": digest,
            "bytes": blob.stat().st_size,
            "runtime": runtime,
            "imports": sorted(got),
            "source": f"areev-tools/{tool.replace('.', '-')}",
        }
    rendered = json.dumps(out, indent=2, sort_keys=False) + "\n"
    published = DIST / "blessed.json"
    if check:
        current = published.read_text() if published.exists() else ""
        if current != rendered:
            failures.append(
                "dist/blessed.json does not describe the committed blobs — run "
                "areev-tools/build.sh and commit the result"
            )
    if failures:
        for f in failures:
            print(f"FAIL {f}", file=sys.stderr)
        raise SystemExit(1)
    if not check:
        published.write_text(rendered)
    for tool, meta in out["tools"].items():
        print(f"{tool:14s} {meta['sha256']}  {meta['bytes']:>6} bytes  {' '.join(meta['imports'])}")


if __name__ == "__main__":
    main()
