# examples/analyzers — bring-your-own command analyzers

`areev loop run --analyzer-cmd 'CMD'` registers a subprocess analyzer written
in any language: a live-grain snapshot arrives on stdin, advisory findings go
back on stdout. It runs at **trust class `command`, auto-apply `never`** — a
domain-specific check (PII, house style, compliance) can *surface* an issue a
human then reviews, but can never mutate memory. A failing command skips that
analyzer for the run, never the pass.

| File | What |
|---|---|
| [`pii_scan.py`](pii_scan.py) | Flags facts whose object looks like an email address (protocol documented inline) |

Try it against the demo corpus:

```bash
areev init --db demo.db --template demo
areev add --db demo.db --ns caller --subject support --relation contact --object "help@example.com"
areev loop run  --db demo.db --analyzer-cmd 'python3 examples/analyzers/pii_scan.py'
areev loop list --db demo.db          # the [external] finding sits in the queue
```

From the bindings, `analyzer_cmd` on `loop_run` is the same seam — and the
only custom-analyzer path from Python/Node (which can't implement the Rust
`Analyzer` trait). Full contract: the module doc of `crates/areev-loop/src/external.rs`
and [`docs/loop.md`](../../docs/loop.md#external-analyzers-optional).
