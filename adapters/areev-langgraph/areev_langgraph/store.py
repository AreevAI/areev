"""AreevStore — LangGraph's long-term memory `BaseStore` over one Areev
memory file.

Mapping: item → Fact head. `put` of an existing (namespace, key)
SUPERSEDES the head (the edit history is the supersession chain, visible
on every Areev surface); `delete` is a tombstone; nothing is ever
overwritten in place. `created_at` is denormalized FORWARD on supersede
(O(1) gets; the chain walk stays available for provenance), `updated_at`
is the head grain's own timestamp.

Search: `filter` supports plain equality plus the operator forms
(`$eq/$ne/$gt/$gte/$lt/$lte`), post-filtered adapter-side over a bounded
candidate pool (`CANDIDATE_POOL`, documented — a prefix holding more
items than the pool can miss matches beyond it). A natural-language
`query` ranks candidates through Areev's hybrid `search` (BM25 + the
vector leg when the file has an embedder); items the ranker does not
reach keep recency order behind the ranked ones.

TTL is NOT supported (`supports_ttl = False`): grains do not expire —
retention policy is declarative and audited (`areev retention`), never a
per-item timer. `ttl`/`refresh_ttl` arguments are accepted and ignored.
"""

from __future__ import annotations

import json
import threading
from collections.abc import Iterable
from datetime import datetime, timezone
from typing import Any, Literal

import areev
from langgraph.store.base import (
    BaseStore,
    GetOp,
    Item,
    ListNamespacesOp,
    MatchCondition,
    Op,
    PutOp,
    Result,
    SearchItem,
    SearchOp,
)

from ._codec import enc, ns_to_str, str_to_ns

_NS = "langgraph"
_ALL = 1_000_000
CANDIDATE_POOL = 1000

_OPS = {
    "$eq": lambda a, b: a == b,
    "$ne": lambda a, b: a != b,
    "$gt": lambda a, b: _cmp(a, b, "__gt__"),
    "$gte": lambda a, b: _cmp(a, b, "__ge__"),
    "$lt": lambda a, b: _cmp(a, b, "__lt__"),
    "$lte": lambda a, b: _cmp(a, b, "__le__"),
}


def _cmp(a: Any, b: Any, op: str) -> bool:
    try:
        return bool(getattr(a, op)(b))
    except (TypeError, AttributeError):
        return False


def _matches(value: dict[str, Any], filter: dict[str, Any]) -> bool:
    for key, expected in filter.items():
        actual = value.get(key)
        if isinstance(expected, dict) and any(k in _OPS for k in expected):
            for op_name, operand in expected.items():
                op = _OPS.get(op_name)
                if op is None or not op(actual, operand):
                    return False
        elif actual != expected:
            return False
    return True


def _ts(ms: int) -> datetime:
    return datetime.fromtimestamp(ms / 1000.0, tz=timezone.utc)


class AreevStore(BaseStore):
    """LangGraph `BaseStore` over one shared Areev memory file."""

    supports_ttl = False

    def __init__(self, path: str) -> None:
        self.db = areev.Areev(path, ns=_NS)
        self._lock = threading.RLock()

    # ---- primitives --------------------------------------------------------

    def _subject(self, namespace: tuple[str, ...]) -> str:
        return f"it:{ns_to_str(namespace)}"

    def _head(self, namespace: tuple[str, ...], key: str) -> dict | None:
        raw = self.db.latest(self._subject(namespace), f"lg:it:{enc(key)}", ns=_NS)
        return json.loads(raw) if raw is not None else None

    def _item(self, namespace: tuple[str, ...], key: str, row: dict) -> Item:
        f = row["fields"]
        created_ms = int(f.get("item_created_ms", f["created_at"]))
        return Item(
            value=json.loads(f["object"]),
            key=key,
            namespace=namespace,
            created_at=_ts(created_ms),
            updated_at=_ts(int(f["created_at"])),
        )

    def _namespaces(self) -> list[tuple[str, ...]]:
        rows = json.loads(self.db.recall("ix:store-ns", k=_ALL, ns=_NS))
        out = []
        for row in rows:
            rel = row["fields"]["relation"]
            if rel.startswith("lg:sns:"):
                out.append(str_to_ns(rel[len("lg:sns:") :]))
        return sorted(out)

    def _items_in(self, namespace: tuple[str, ...]) -> list[tuple[str, dict]]:
        rows = json.loads(self.db.recall(self._subject(namespace), k=_ALL, ns=_NS))
        out = []
        for row in rows:
            rel = row["fields"]["relation"]
            if rel.startswith("lg:it:"):
                out.append((str_to_ns(rel[len("lg:it:") :])[0], row))
        return out

    # ---- op execution ------------------------------------------------------

    def _do_get(self, op: GetOp) -> Item | None:
        row = self._head(op.namespace, op.key)
        if row is None:
            return None
        return self._item(op.namespace, op.key, row)

    def _do_put(self, op: PutOp) -> None:
        subject = self._subject(op.namespace)
        relation = f"lg:it:{enc(op.key)}"
        head = self._head(op.namespace, op.key)
        if op.value is None:
            # BaseStore routes delete as a PutOp with value None → tombstone.
            if head is not None:
                self.db.forget(head["hash"])
            return
        fields = {
            "subject": subject,
            "relation": relation,
            "object": json.dumps(op.value),
        }
        if head is not None:
            f = head["fields"]
            # created_at denormalized forward: O(1) reads, chain for lineage.
            fields["item_created_ms"] = int(f.get("item_created_ms", f["created_at"]))
            if f["object"] != fields["object"]:
                self.db.supersede(head["hash"], "fact", json.dumps(fields), ns=_NS)
        else:
            self.db.add("fact", json.dumps(fields), ns=_NS)
            self.db.add_fact(
                "ix:store-ns",
                f"lg:sns:{ns_to_str(op.namespace)}",
                "1",
                ns=_NS,
                idempotent=True,
            )

    def _do_search(self, op: SearchOp) -> list[SearchItem]:
        prefix = op.namespace_prefix
        candidates: list[tuple[tuple[str, ...], str, dict]] = []
        for namespace in self._namespaces():
            if namespace[: len(prefix)] != tuple(prefix):
                continue
            for key, row in self._items_in(namespace):
                candidates.append((namespace, key, row))
                if len(candidates) >= CANDIDATE_POOL:
                    break
            if len(candidates) >= CANDIDATE_POOL:
                break
        if op.filter:
            candidates = [
                (namespace, key, row)
                for namespace, key, row in candidates
                if _matches(json.loads(row["fields"]["object"]), op.filter)
            ]
        scored: list[tuple[float | None, tuple[str, ...], str, dict]] = []
        if op.query:
            ranked = json.loads(self.db.search(op.query, k=CANDIDATE_POOL, ns=_NS))
            order: dict[str, int] = {}
            for i, hit in enumerate(ranked):
                order[hit.get("hash", "")] = i
            in_rank = []
            out_rank = []
            for namespace, key, row in candidates:
                pos = order.get(row["hash"])
                if pos is not None:
                    in_rank.append((pos, namespace, key, row))
                else:
                    out_rank.append((namespace, key, row))
            in_rank.sort(key=lambda t: t[0])
            for pos, namespace, key, row in in_rank:
                scored.append((1.0 / (1 + pos), namespace, key, row))
            out_rank.sort(key=lambda t: int(t[2]["fields"]["created_at"]), reverse=True)
            for namespace, key, row in out_rank:
                scored.append((None, namespace, key, row))
        else:
            candidates.sort(
                key=lambda t: int(t[2]["fields"]["created_at"]), reverse=True
            )
            scored = [(None, n, k, r) for n, k, r in candidates]
        page = scored[op.offset : op.offset + op.limit]
        out = []
        for score, namespace, key, row in page:
            item = self._item(namespace, key, row)
            out.append(
                SearchItem(
                    namespace=namespace,
                    key=key,
                    value=item.value,
                    created_at=item.created_at,
                    updated_at=item.updated_at,
                    score=score,
                )
            )
        return out

    def _do_list_namespaces(self, op: ListNamespacesOp) -> list[tuple[str, ...]]:
        namespaces = self._namespaces()

        def cond_ok(namespace: tuple[str, ...], cond: MatchCondition) -> bool:
            path = tuple(cond.path)
            if len(path) > len(namespace):
                return False
            segment = (
                namespace[: len(path)]
                if cond.match_type == "prefix"
                else namespace[-len(path) :]
            )
            return all(p == "*" or p == s for p, s in zip(path, segment))

        if op.match_conditions:
            namespaces = [
                n for n in namespaces if all(cond_ok(n, c) for c in op.match_conditions)
            ]
        if op.max_depth is not None:
            seen: dict[tuple[str, ...], None] = {}
            for n in namespaces:
                seen.setdefault(n[: op.max_depth], None)
            namespaces = list(seen)
        return namespaces[op.offset : op.offset + op.limit]

    # ---- the abstract surface ---------------------------------------------

    def batch(self, ops: Iterable[Op]) -> list[Result]:
        results: list[Result] = []
        with self._lock:
            # Sequential execution IS read-your-writes: a GetOp after a
            # PutOp in the same batch sees the put.
            for op in ops:
                if isinstance(op, GetOp):
                    results.append(self._do_get(op))
                elif isinstance(op, PutOp):
                    results.append(self._do_put(op))
                elif isinstance(op, SearchOp):
                    results.append(self._do_search(op))
                elif isinstance(op, ListNamespacesOp):
                    results.append(self._do_list_namespaces(op))
                else:  # pragma: no cover - future op types fail loudly
                    raise NotImplementedError(f"unsupported op {type(op).__name__}")
        return results

    async def abatch(self, ops: Iterable[Op]) -> list[Result]:
        return self.batch(ops)
