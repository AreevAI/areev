"""AreevStorageBackend — CrewAI memory over one Areev memory file.

The mapping that makes governance real (governed-agents §8 Wave 3):

- **update → supersession, delete → tombstone.** CrewAI's
  ConsolidationFlow LLM-rewrites records on save; here every rewrite is a
  supersession chain, so "what did the agent believe before consolidation"
  is a query, not a shrug. Every READ method is pinned heads-only.
- **`source` → subject identity.** A record whose ``source`` names an
  identity is stored under the partition-keyed subject
  ``<source>#crw:<id>`` — Areev's erasure selector matches partition
  keys, so ``FORGET SUBJECT "<source>"`` (CAL, CLI, or
  ``Areev.forget_subject``) erases those records, their supersession
  history, AND their index rows (which reference the record subject in
  object position), with a receipt. That is the right-to-erasure demo.
- **`search` consumes CrewAI's pre-computed embedding** through
  `nearest_vector` — re-embedding with a different model would be
  silently-wrong similarity. The embedding dimension is recorded in file
  meta on first write; a mismatch raises (``VAL``).
- **Predicate deletes** (`delete(filter…)` and `reset(scope)` — scope is
  a predicate too) enumerate matching heads and tombstone each by hash;
  one sweep summary Fact is recorded on top of the per-grain Tier-2 audit
  records the facade already writes. Stated honestly: resetting 100k
  records writes 100k tombstones (minutes, not ms) and the file does not
  shrink — per-memory crypto-erasure cannot substitute (the key is
  per-memory, not per-scope); sharding hot scopes into files is the
  documented alternative.
- **`private` is enforced store-side:** private records only surface when
  the caller's ``scope_prefix`` covers their scope — a cross-scope search
  never leaks them.

Deviations and bounds, stated:

- ``last_accessed`` is data the caller sets (via ``update``), never a
  write-on-read — reads stay reads in an append-only store.
- A supersession WITHOUT a fresh ``embedding`` leaves the new head
  vector-less (embeddings are index-layer and do not carry across
  supersession): supply the embedding on every save/update whose record
  should stay findable by vector search — CrewAI's own flows do.
- ``source`` rides RAW in the record subject (``<source>#crw:<id>``) so
  ``FORGET SUBJECT <source>`` works. The erasure selector matches
  partition keys at any non-alphanumeric boundary, so a source that is a
  prefix of another source (``user`` vs ``user#1``) widens the blast
  radius — choose sources that are not prefixes of each other.
"""

from __future__ import annotations

import json
import threading
import time
from datetime import datetime, timezone
from typing import Any
from urllib.parse import quote

import areev
from crewai.memory.storage.backend import StorageBackend  # noqa: F401 (protocol)
from crewai.memory.types import MemoryRecord, ScopeInfo

_NS = "crewai"
_ALL = 1_000_000


def _enc(component: str) -> str:
    return quote(component, safe="")


def _iso(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).isoformat()


def _from_iso(text: str) -> datetime:
    return datetime.fromisoformat(text)


def _scope_matches(scope: str, prefix: str | None) -> bool:
    if prefix is None or prefix == "/" or prefix == "":
        return True
    prefix = prefix.rstrip("/")
    return scope == prefix or scope.startswith(prefix + "/")


class AreevStorageBackend:
    """CrewAI `StorageBackend` over one Areev memory file."""

    def __init__(self, path: str) -> None:
        self.db = areev.Areev(path, ns=_NS)
        self._lock = threading.RLock()

    # ---- encoding ----------------------------------------------------------

    def _subject_for(self, record: MemoryRecord) -> str:
        if record.source:
            # Partition key of the source identity: FORGET SUBJECT <source>
            # selects this record, its history, and its index rows.
            return f"{record.source}#crw:{record.id}"
        return f"crw:{record.id}"

    def _record_fields(self, record: MemoryRecord, subject: str) -> dict:
        payload = {
            "id": record.id,
            "content": record.content,
            "scope": record.scope,
            "categories": list(record.categories),
            "metadata": dict(record.metadata),
            "importance": record.importance,
            "created_at": _iso(record.created_at),
            "last_accessed": _iso(record.last_accessed),
            "source": record.source,
            "private": record.private,
        }
        return {"subject": subject, "relation": "crw:record", "object": json.dumps(payload)}

    def _parse(self, row: dict) -> MemoryRecord:
        payload = json.loads(row["fields"]["object"])
        return MemoryRecord(
            id=payload["id"],
            content=payload["content"],
            scope=payload["scope"],
            categories=payload["categories"],
            metadata=payload["metadata"],
            importance=payload["importance"],
            created_at=_from_iso(payload["created_at"]),
            last_accessed=_from_iso(payload["last_accessed"]),
            embedding=None,  # index-layer, never in the grain blob
            source=payload["source"],
            private=payload["private"],
        )

    def _head_for_id(self, record_id: str) -> dict | None:
        link = self.db.latest("ix:by-id", f"crw:id:{record_id}", ns=_NS)
        if link is None:
            return None
        subject = json.loads(link)["fields"]["object"]
        raw = self.db.latest(subject, "crw:record", ns=_NS)
        return json.loads(raw) if raw is not None else None

    def _heads(self, scope_prefix: str | None = None) -> list[dict]:
        """Every current record head (optionally scope-filtered) — the
        heads-only read every method routes through."""
        out: list[dict] = []
        for scope in self._scopes():
            if not _scope_matches(scope, scope_prefix):
                continue
            rows = json.loads(self.db.recall(f"ix:scope:{_enc(scope)}", k=_ALL, ns=_NS))
            for row in rows:
                if not row["fields"]["relation"].startswith("crw:in:"):
                    continue
                subject = row["fields"]["object"]
                raw = self.db.latest(subject, "crw:record", ns=_NS)
                if raw is not None:
                    out.append(json.loads(raw))
        return out

    def _scopes(self) -> list[str]:
        rows = json.loads(self.db.recall("ix:scopes", k=_ALL, ns=_NS))
        out = []
        for row in rows:
            rel = row["fields"]["relation"]
            if rel.startswith("crw:scope:"):
                from urllib.parse import unquote

                out.append(unquote(rel[len("crw:scope:") :]))
        return sorted(out)

    # ---- writes ------------------------------------------------------------

    def save(self, records: list[MemoryRecord]) -> None:
        with self._lock:
            for record in records:
                subject = self._subject_for(record)
                fields = self._record_fields(record, subject)
                head = self.db.latest(subject, "crw:record", ns=_NS)
                if head is not None:
                    old = json.loads(head)
                    # ConsolidationFlow rewrites records THROUGH save — a
                    # scope change here must move the scope index exactly
                    # like update() does, or the record becomes invisible to
                    # its new scope and undeletable by any scope predicate.
                    old_scope = json.loads(old["fields"]["object"])["scope"]
                    if record.scope != old_scope:
                        link = self.db.latest(
                            f"ix:scope:{_enc(old_scope)}", f"crw:in:{record.id}", ns=_NS
                        )
                        if link is not None:
                            self.db.forget(json.loads(link)["hash"])
                        self.db.add_fact(
                            f"ix:scope:{_enc(record.scope)}", f"crw:in:{record.id}", subject,
                            ns=_NS, idempotent=True,
                        )
                        self.db.add_fact(
                            "ix:scopes", f"crw:scope:{_enc(record.scope)}", "1",
                            ns=_NS, idempotent=True,
                        )
                    if old["fields"]["object"] != fields["object"]:
                        new_hash = self.db.supersede(
                            old["hash"], "fact", json.dumps(fields), ns=_NS
                        )
                    else:
                        new_hash = old["hash"]
                else:
                    new_hash = self.db.add("fact", json.dumps(fields), ns=_NS)
                    self.db.add_fact("ix:by-id", f"crw:id:{record.id}", subject, ns=_NS, idempotent=True)
                    self.db.add_fact(f"ix:scope:{_enc(record.scope)}", f"crw:in:{record.id}", subject, ns=_NS, idempotent=True)
                    self.db.add_fact("ix:scopes", f"crw:scope:{_enc(record.scope)}", "1", ns=_NS, idempotent=True)
                if record.embedding:
                    # The HEAD carries the indexed embedding; a superseded
                    # grain's stale vector is filtered heads-only at read.
                    self.db.add_embedding(new_hash, [float(x) for x in record.embedding])

    def update(self, record: MemoryRecord) -> None:
        with self._lock:
            head = self._head_for_id(record.id)
            if head is None:
                self.save([record])
                return
            subject = head["fields"]["subject"]
            fields = self._record_fields(record, subject)
            if head["fields"]["object"] != fields["object"]:
                new_hash = self.db.supersede(head["hash"], "fact", json.dumps(fields), ns=_NS)
            else:
                new_hash = head["hash"]
            old_scope = json.loads(head["fields"]["object"])["scope"]
            if record.scope != old_scope:
                link = self.db.latest(f"ix:scope:{_enc(old_scope)}", f"crw:in:{record.id}", ns=_NS)
                if link is not None:
                    self.db.forget(json.loads(link)["hash"])
                self.db.add_fact(f"ix:scope:{_enc(record.scope)}", f"crw:in:{record.id}", subject, ns=_NS, idempotent=True)
                self.db.add_fact("ix:scopes", f"crw:scope:{_enc(record.scope)}", "1", ns=_NS, idempotent=True)
            if record.embedding:
                self.db.add_embedding(new_hash, [float(x) for x in record.embedding])

    # ---- reads (heads-only, pinned) ----------------------------------------

    def search(
        self,
        query_embedding: list[float],
        scope_prefix: str | None = None,
        categories: list[str] | None = None,
        metadata_filter: dict[str, Any] | None = None,
        limit: int = 10,
        min_score: float = 0.0,
    ) -> list[tuple[MemoryRecord, float]]:
        with self._lock:
            hits = json.loads(
                self.db.nearest_vector(
                    [float(x) for x in query_embedding],
                    relation="crw:record",
                    k=max(limit * 4, 32),  # oversample; head/policy filters follow
                    ns=_NS,
                )
            )
            # Join hits against CURRENT heads by hash: a stale vector on a
            # superseded grain, or an erased record, simply never joins —
            # heads-only is structural, not a comparison.
            heads_by_hash = {h["hash"]: h for h in self._heads(None)}
            out: list[tuple[MemoryRecord, float]] = []
            for hit in hits:
                score = float(hit.get("similarity", 0.0))
                if score < min_score:
                    continue
                head = heads_by_hash.get(hit["hash"])
                if head is None:
                    continue  # superseded or erased: heads only, always
                record = self._parse(head)
                if record.private and not (
                    scope_prefix is not None and _scope_matches(record.scope, scope_prefix)
                ):
                    continue  # store-side privacy: no cross-scope leaks
                if not _scope_matches(record.scope, scope_prefix):
                    continue
                if categories and not set(categories) & set(record.categories):
                    continue
                if metadata_filter and any(
                    record.metadata.get(k) != v for k, v in metadata_filter.items()
                ):
                    continue
                out.append((record, score))
                if len(out) >= limit:
                    break
            return out

    def get_record(self, record_id: str) -> MemoryRecord | None:
        with self._lock:
            head = self._head_for_id(record_id)
            return self._parse(head) if head is not None else None

    def list_records(
        self,
        scope_prefix: str | None = None,
        limit: int = 200,
        offset: int = 0,
    ) -> list[MemoryRecord]:
        with self._lock:
            records = [self._parse(h) for h in self._heads(scope_prefix)]
            records.sort(key=lambda r: r.created_at, reverse=True)
            return records[offset : offset + limit]

    def get_scope_info(self, scope: str) -> ScopeInfo:
        with self._lock:
            records = [self._parse(h) for h in self._heads(scope)]
            direct = [r for r in records if r.scope == scope]
            categories = sorted({c for r in direct for c in r.categories})
            children = sorted(
                {
                    scope.rstrip("/") + "/" + r.scope[len(scope.rstrip("/") + "/") :].split("/")[0]
                    for r in records
                    if r.scope != scope and _scope_matches(r.scope, scope)
                }
            )
            return ScopeInfo(
                path=scope,
                record_count=len(direct),
                categories=categories,
                oldest_record=min((r.created_at for r in direct), default=None),
                newest_record=max((r.created_at for r in direct), default=None),
                child_scopes=children,
            )

    def list_scopes(self, parent: str = "/") -> list[str]:
        with self._lock:
            parent_norm = parent.rstrip("/")
            children = set()
            for scope in self._scopes():
                if scope == parent or not _scope_matches(scope, parent):
                    continue
                rest = scope[len(parent_norm) + 1 :]
                children.add(parent_norm + "/" + rest.split("/")[0])
            return sorted(children)

    def list_categories(self, scope_prefix: str | None = None) -> dict[str, int]:
        with self._lock:
            counts: dict[str, int] = {}
            for head in self._heads(scope_prefix):
                for category in json.loads(head["fields"]["object"])["categories"]:
                    counts[category] = counts.get(category, 0) + 1
            return counts

    def count(self, scope_prefix: str | None = None) -> int:
        with self._lock:
            return len(self._heads(scope_prefix))

    # ---- destruction (audited) ---------------------------------------------

    def delete(
        self,
        scope_prefix: str | None = None,
        categories: list[str] | None = None,
        record_ids: list[str] | None = None,
        older_than: datetime | None = None,
        metadata_filter: dict[str, Any] | None = None,
    ) -> int:
        with self._lock:
            victims: list[dict] = []
            if record_ids is not None:
                for record_id in record_ids:
                    head = self._head_for_id(record_id)
                    if head is not None:
                        victims.append(head)
            else:
                victims = self._heads(scope_prefix)
            selected = []
            for head in victims:
                record = self._parse(head)
                if not _scope_matches(record.scope, scope_prefix):
                    continue
                if categories and not set(categories) & set(record.categories):
                    continue
                if older_than is not None and record.created_at >= older_than:
                    continue
                if metadata_filter and any(
                    record.metadata.get(k) != v for k, v in metadata_filter.items()
                ):
                    continue
                selected.append((head, record))
            for head, record in selected:
                self._tombstone(head, record)
            if selected:
                self._sweep_audit(
                    "delete",
                    len(selected),
                    {
                        "scope_prefix": self._fingerprint(scope_prefix),
                        "categories": categories,
                        "record_ids_given": record_ids is not None,
                        "older_than": _iso(older_than) if older_than else None,
                        "metadata_filter_keys": sorted(metadata_filter) if metadata_filter else [],
                    },
                )
            return len(selected)

    def reset(self, scope_prefix: str | None = None) -> None:
        # Scope IS a predicate: same enumerate-and-tombstone path, same
        # audit trail — never a structural shortcut around CAL's invariant.
        self.delete(scope_prefix=scope_prefix)

    def _tombstone(self, head: dict, record: MemoryRecord) -> None:
        self.db.forget(head["hash"])
        link = self.db.latest("ix:by-id", f"crw:id:{record.id}", ns=_NS)
        if link is not None:
            self.db.forget(json.loads(link)["hash"])
        member = self.db.latest(f"ix:scope:{_enc(record.scope)}", f"crw:in:{record.id}", ns=_NS)
        if member is not None:
            self.db.forget(json.loads(member)["hash"])

    @staticmethod
    def _fingerprint(value):
        """Identity-bearing criteria are recorded as fingerprints, never
        verbatim — an immortal audit record naming the erased party would
        undo the erasure it records (the same rule as the core's
        subject_fingerprint)."""
        if value is None:
            return None
        import hashlib as _hl

        return "fp:" + _hl.sha256(str(value).encode()).hexdigest()[:16]

    def _sweep_audit(self, kind: str, count: int, criteria: dict) -> None:
        self.db.add(
            "fact",
            json.dumps(
                {
                    "subject": "audit:crewai",
                    "relation": f"crw:sweep:{int(time.time() * 1000)}",
                    "object": json.dumps({"op": kind, "erased": count, "criteria": criteria}),
                }
            ),
            ns=_NS,
        )

    # ---- async mirrors ------------------------------------------------------

    async def asave(self, records: list[MemoryRecord]) -> None:
        return self.save(records)

    async def asearch(
        self,
        query_embedding: list[float],
        scope_prefix: str | None = None,
        categories: list[str] | None = None,
        metadata_filter: dict[str, Any] | None = None,
        limit: int = 10,
        min_score: float = 0.0,
    ) -> list[tuple[MemoryRecord, float]]:
        return self.search(
            query_embedding, scope_prefix, categories, metadata_filter, limit, min_score
        )

    async def adelete(
        self,
        scope_prefix: str | None = None,
        categories: list[str] | None = None,
        record_ids: list[str] | None = None,
        older_than: datetime | None = None,
        metadata_filter: dict[str, Any] | None = None,
    ) -> int:
        return self.delete(scope_prefix, categories, record_ids, older_than, metadata_filter)
