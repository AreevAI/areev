"""The documented "proof" blocks are executable, and their printed output is asserted.

Two bug reports against the same six lines were the same failure twice over:
#40 raised `STO-E001` where the docs showed a result, and the fix for it (a
duplicate add became a no-op) turned the crash into silence — the block ran
clean and printed nothing, which reads as "Areev Loop found no problems" (#66).
Both would have been caught here.

The scope is deliberately narrow: the Areev Loop proof blocks, introduced as
"the fastest way to see it needs no agent". They are extracted from the shipped
Markdown rather than retyped, so a block that is edited without being re-run
fails instead of quietly drifting from the engine.

`README.md` used to carry one and no longer does: the 1.5.1 revamp made the
README visual-first, leading with the CLI and console screenshots rather than a
Python listing. This file kept asserting a block that had been removed, so the
Python job went red on a doc change — the guard outliving the thing it guarded.
The guard itself still matters, so it moved down to the two docs that do carry
a block rather than being deleted.
"""

import json
import re
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[3]
DOCS = ["docs/loop.md", "docs/cookbook.md"]

# Lines that are prose-in-code: a `<hash>` placeholder stands in for a real
# address because a 64-hex string is unreadable in a doc.
PLACEHOLDER = re.compile(r"<[a-z_]+>")


def python_fences(md: str):
    """Every ```python fence in a doc, with its 1-based starting line."""
    out, current, start = [], None, 0
    for i, line in enumerate(md.splitlines(), start=1):
        stripped = line.rstrip()
        if current is None and stripped == "```python":
            current, start = [], i + 1
        elif current is not None and stripped == "```":
            out.append((start, "\n".join(current)))
            current = None
        elif current is not None:
            current.append(line)
    return out


def proof_blocks():
    """The self-contained blocks that exercise the loop.

    `import areev` is what separates a runnable demonstration from the API
    listings elsewhere in the same docs, which show call shapes against a `db`
    the reader is assumed to already have.
    """
    found = []
    for rel in DOCS:
        text = (REPO / rel).read_text(encoding="utf-8")
        for line_no, body in python_fences(text):
            if all(t in body for t in ("import areev", "record_tool_call", "recommendations(")):
                found.append(pytest.param(rel, line_no, body, id=f"{rel}:{line_no}"))
    return found


BLOCKS = proof_blocks()


def test_the_docs_still_contain_the_proof_blocks():
    """Guard the guard: a renamed method or a deleted fence must not silently
    reduce this file to zero tests."""
    assert len(BLOCKS) == len(DOCS), (
        f"expected one loop proof block in each of {DOCS}, found {len(BLOCKS)}"
    )


@pytest.mark.parametrize("rel,line_no,body", BLOCKS)
def test_documented_proof_block_prints_what_it_claims(rel, line_no, body, tmp_path, capsys, monkeypatch):
    # The blocks open a relative "proof.db"; run them somewhere disposable.
    monkeypatch.chdir(tmp_path)

    # Drop the placeholder lines — `apply_recommendation(<hash>, …)` is prose
    # showing the call shape, not a runnable statement. Everything else runs.
    runnable = "\n".join(l for l in body.splitlines() if not PLACEHOLDER.search(l))

    exec(compile(runnable, f"{rel}:{line_no}", "exec"), {"__name__": "__main__"})

    printed = capsys.readouterr().out
    assert printed.strip(), (
        f"{rel}:{line_no} printed nothing. The block is introduced as a "
        f"demonstration that the loop finds something — an empty pending queue "
        f"is the #66 regression, not a passing example."
    )
    # The documented finding: a high-severity cluster naming the tool, the
    # count, and the rate.
    assert "high" in printed, f"{rel}:{line_no} → {printed!r}"
    assert "stripe_refund" in printed, f"{rel}:{line_no} → {printed!r}"
    assert "5 times" in printed, (
        f"{rel}:{line_no} must report all five failures, not a deduplicated "
        f"count → {printed!r}"
    )
    assert "71%" in printed, f"{rel}:{line_no} → {printed!r}"


def test_the_docs_print_the_output_they_claim():
    """A block that prints its expected output in a `# →` comment must keep it
    true. This used to read `README.md` alone, which no longer carries a proof
    block; asserting over the docs that do keeps the check where the claim is."""
    for rel in DOCS:
        text = (REPO / rel).read_text(encoding="utf-8")
        claims = [l for l in text.splitlines() if l.startswith("# → high")]
        if not claims:
            continue
        for claim in claims:
            for token in ("stripe_refund", "5 times", "71%", "rate_limited"):
                assert token in claim, f"{rel}'s documented output lost {token!r}: {claim!r}"


def test_loop_doc_block_leaves_two_recommendations(tmp_path, monkeypatch):
    """`docs/loop.md` indexes `pending[1]`, so the block needs a second finding
    (the contradiction) alongside the tool-failure cluster. With #66 open the
    queue held at most one and the line was an IndexError."""
    import areev

    monkeypatch.chdir(tmp_path)
    db = areev.Areev("loopdoc.db", actor="user:me")
    for _ in range(5):
        db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
    for _ in range(2):
        db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)
    db.add_fact("acme", "deploy_target", "us-east-1", 0.9)
    db.add_fact("acme", "deploy_target", "eu-west-1", 0.9)
    db.loop_run()

    pending = json.loads(db.recommendations('{"status":"pending"}'))
    assert len(pending) >= 2, f"pending[1] must exist, got {len(pending)}"
    analyzers = {r["analyzer"].split("/")[0] for r in pending}
    assert "loop.tool_failure" in analyzers
