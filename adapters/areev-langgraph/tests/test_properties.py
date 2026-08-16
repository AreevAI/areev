"""The [R3] property tests: namespace encoding is reversible for hostile
identifiers, and the version counter's lexicographic order IS its numeric
order — the two invariants everything else silently leans on."""

from __future__ import annotations

import random
import string

from areev_langgraph._codec import dec, enc, ns_to_str, str_to_ns
from areev_langgraph.checkpoint import AreevCheckpointSaver


HOSTILE = [
    "",
    "plain",
    "with space",
    "with/slash",
    "with:colon",
    "with%percent",
    "with%2Fencoded-slash",
    "ünïcode-✓",
    "tab\tand\nnewline",
    "..",
    "a" * 200,
]


def test_component_encoding_round_trips():
    for s in HOSTILE:
        assert dec(enc(s)) == s
    # And encoded components never contain the separators they ride inside.
    for s in HOSTILE:
        if s:
            assert "/" not in enc(s)
            assert ":" not in enc(s)


def test_namespace_tuples_round_trip_and_never_collide():
    rng = random.Random(7)
    seen = {}
    for _ in range(500):
        depth = rng.randint(1, 4)
        ns = tuple(
            "".join(rng.choice(string.printable[:80]) for _ in range(rng.randint(0, 12)))
            for _ in range(depth)
        )
        path = ns_to_str(ns)
        assert str_to_ns(path) == ns
        assert seen.setdefault(path, ns) == ns, "distinct tuples map to distinct paths"
    # The classic boundary-forgery case, explicitly:
    assert ns_to_str(("a", "b/c")) != ns_to_str(("a/b", "c"))
    assert ns_to_str(("a", "b:c")) != ns_to_str(("a:b", "c"))


def test_get_next_version_lexicographic_is_numeric(tmp_path):
    saver = AreevCheckpointSaver(str(tmp_path))
    versions = []
    current = None
    for _ in range(1200):
        current = saver.get_next_version(current, None)
        versions.append(current)
    assert versions == sorted(versions), "lexicographic order must equal numeric order"
    assert len(set(versions)) == len(versions)
    # The reference format survives: int-prefix parse, re-pad to 32 digits.
    assert versions[0].startswith(f"{1:032}.")
    assert saver.get_next_version("0000000000000000000000000000009.5", None).startswith(
        f"{10:032}."
    )
