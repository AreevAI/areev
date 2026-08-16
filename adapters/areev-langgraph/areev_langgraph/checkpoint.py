"""AreevCheckpointSaver — LangGraph checkpoints as content-addressed grains.

Deployment mapping (governed-agents §8 Wave 3, pinned): **one LangGraph
thread = one Areev memory file** — their thread is our isolation unit, so
`delete_thread` is the erasure of one file, checkpoint history replicates
and forks with the file, and the single-writer rule is satisfied per
thread. Handles live in a process-wide LRU (close-on-evict) so unbounded
thread counts cannot exhaust file handles.

Identity (the [R3] blocker fix): a checkpoint is keyed
`(thread_id, checkpoint_ns, checkpoint_id)` and checkpoints form a TREE —
one grain identity per checkpoint id, supersession ONLY for a re-put of
the same id, `parent_checkpoint_id` stored so `parent_config`
reconstructs, and `list()` never chain-collapses (chain-collapsing would
destroy the time-travel this backend exists to strengthen).

Storage shape (per thread file, namespace "langgraph"):

- checkpoint: `ck:<ns>` / `lg:ck:<id>` — object = base64 of the
  typed-serialized checkpoint (channel_values SPLIT OUT, as the reference
  saver does); extras: `ck_type`, `md_type`/`md_b64` (typed metadata),
  `parent_id`, and `run_id` lifted top-level when present (this is what
  `delete_for_runs` selects on — and what joins LangGraph runs to
  `runs_touching` for free).
- channel blob: `bl:<ns>:<channel>` / `lg:bl:<version>` — one value per
  (channel, version); the `empty` sentinel rides the type tag.
- pending write: `wr:<ns>:<ckpt_id>` / `lg:wr:<task>:<idx>` — upsert keyed
  exactly (thread, ns, checkpoint_id, task_id, idx): a retry with
  different bytes SUPERSEDES (never appends); regular writes (idx >= 0)
  are first-commit-wins, special writes (idx < 0) overwrite — the
  reference semantics.
- namespace index: `ix:ns` / `lg:ns:<ns>` — enumeration without scans.
"""

from __future__ import annotations

import os
import threading
from collections import OrderedDict
from collections.abc import AsyncIterator, Iterator, Mapping, Sequence
from typing import Any

import areev
from langchain_core.runnables import RunnableConfig
from langgraph.checkpoint.base import (
    WRITES_IDX_MAP,
    BaseCheckpointSaver,
    ChannelVersions,
    Checkpoint,
    CheckpointMetadata,
    CheckpointTuple,
    DeltaChannelHistory,
    PendingWrite,
    SerializerProtocol,
    get_checkpoint_id,
    get_checkpoint_metadata,
)

from ._codec import b64, dec, enc, unb64

_NS = "langgraph"
_ALL = 1_000_000  # enumeration bound for recall(); far above any sane thread


def _fields(row: dict) -> dict:
    return row["fields"]


class AreevCheckpointSaver(BaseCheckpointSaver[str]):
    """A LangGraph checkpointer over per-thread Areev memory files.

    Args:
        root: Directory holding one ``<thread>.db`` per thread.
        serde: Optional serializer (defaults to LangGraph's JsonPlus).
        max_open: LRU capacity for open thread files (close-on-evict).
    """

    def __init__(
        self,
        root: str,
        *,
        serde: SerializerProtocol | None = None,
        max_open: int = 64,
    ) -> None:
        super().__init__(serde=serde)
        self.root = root
        os.makedirs(root, exist_ok=True)
        self._max_open = max_open
        self._handles: OrderedDict[str, areev.Areev] = OrderedDict()
        self._lock = threading.RLock()

    # ---- handles -----------------------------------------------------------

    def _path(self, thread_id: str) -> str:
        # A short hash suffix keeps distinct thread ids distinct on
        # case-insensitive filesystems (default APFS/NTFS), where
        # enc("Alice") and enc("alice") would otherwise be one file.
        import hashlib as _hl

        tag = _hl.sha256(thread_id.encode()).hexdigest()[:8]
        return os.path.join(self.root, f"{enc(thread_id)}-{tag}.db")

    def _db(self, thread_id: str, *, create: bool = True) -> areev.Areev | None:
        with self._lock:
            if thread_id in self._handles:
                self._handles.move_to_end(thread_id)
                return self._handles[thread_id]
            path = self._path(thread_id)
            if not create and not os.path.exists(path):
                return None
            handle = areev.Areev(path, ns=_NS)
            self._handles[thread_id] = handle
            while len(self._handles) > self._max_open:
                _, evicted = self._handles.popitem(last=False)
                del evicted  # handle releases on drop
            return handle

    def _close(self, thread_id: str) -> None:
        with self._lock:
            handle = self._handles.pop(thread_id, None)
            del handle

    def _threads(self) -> list[str]:
        out = []
        for name in sorted(os.listdir(self.root)):
            if not name.endswith(".db") or name.endswith(".telemetry.db"):
                continue
            stem = name[: -len(".db")]
            # Strip the case-fold tag ("-<8 hex>"); legacy untagged files
            # decode as-is.
            if len(stem) > 9 and stem[-9] == "-" and all(c in "0123456789abcdef" for c in stem[-8:]):
                stem = stem[:-9]
            out.append(dec(stem))
        return out

    # ---- enumeration helpers ----------------------------------------------

    def _recall(self, db: areev.Areev, subject: str) -> list[dict]:
        import json

        return json.loads(db.recall(subject, k=_ALL, ns=_NS))

    def _namespaces(self, db: areev.Areev) -> list[str]:
        """RAW index suffixes (marker + encoded ns) — decoded by callers
        that know which marker they strip; decoding a composite string
        would corrupt encodings that contain the marker text."""
        rows = self._recall(db, "ix:ns")
        out = []
        for row in rows:
            rel = _fields(row)["relation"]
            if rel.startswith("lg:ns:"):
                out.append(rel[len("lg:ns:") :])
        return sorted(out)

    def _checkpoints(self, db: areev.Areev, checkpoint_ns: str) -> dict[str, dict]:
        """checkpoint_id -> head row for one namespace."""
        rows = self._recall(db, f"ck:{enc(checkpoint_ns)}")
        out: dict[str, dict] = {}
        for row in rows:
            rel = _fields(row)["relation"]
            if rel.startswith("lg:ck:"):
                out[rel[len("lg:ck:") :]] = row
        return out

    def _writes(self, db: areev.Areev, checkpoint_ns: str, checkpoint_id: str) -> list[dict]:
        rows = self._recall(db, f"wr:{enc(checkpoint_ns)}:{checkpoint_id}")
        rows = [r for r in rows if _fields(r)["relation"].startswith("lg:wr:")]
        # Canonical order: (task_id, idx) — never store arrival order.
        rows.sort(key=lambda r: (_fields(r)["task_id"], int(_fields(r)["idx"])))
        return rows

    def _blob(self, db: areev.Areev, checkpoint_ns: str, channel: str, version: Any) -> tuple[str, bytes] | None:
        head = db.latest(f"bl:{enc(checkpoint_ns)}:{enc(channel)}", f"lg:bl:{version}", ns=_NS)
        if head is None:
            return None
        import json

        f = _fields(json.loads(head))
        return (f["bl_type"], unb64(f["object"]))

    def _load_channel_values(
        self, db: areev.Areev, checkpoint_ns: str, versions: ChannelVersions
    ) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for channel, version in versions.items():
            blob = self._blob(db, checkpoint_ns, channel, version)
            if blob is None or blob[0] == "empty":
                continue
            result[channel] = self.serde.loads_typed(blob)
        return result

    def _tuple_from_row(
        self,
        db: areev.Areev,
        thread_id: str,
        checkpoint_ns: str,
        checkpoint_id: str,
        row: dict,
    ) -> CheckpointTuple:
        f = _fields(row)
        checkpoint: Checkpoint = self.serde.loads_typed((f["ck_type"], unb64(f["object"])))
        metadata = self.serde.loads_typed((f["md_type"], unb64(f["md_b64"])))
        parent_id = f.get("parent_id") or None
        writes = [
            (
                _fields(w)["task_id"],
                _fields(w)["channel"],
                self.serde.loads_typed((_fields(w)["wr_type"], unb64(_fields(w)["object"]))),
            )
            for w in self._writes(db, checkpoint_ns, checkpoint_id)
        ]
        return CheckpointTuple(
            config={
                "configurable": {
                    "thread_id": thread_id,
                    "checkpoint_ns": checkpoint_ns,
                    "checkpoint_id": checkpoint_id,
                }
            },
            checkpoint={
                **checkpoint,
                "channel_values": self._load_channel_values(
                    db, checkpoint_ns, checkpoint["channel_versions"]
                ),
            },
            metadata=metadata,
            parent_config=(
                {
                    "configurable": {
                        "thread_id": thread_id,
                        "checkpoint_ns": checkpoint_ns,
                        "checkpoint_id": parent_id,
                    }
                }
                if parent_id
                else None
            ),
            pending_writes=writes,
        )

    # ---- the core surface --------------------------------------------------

    def get_tuple(self, config: RunnableConfig) -> CheckpointTuple | None:
        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")
        db = self._db(thread_id, create=False)
        if db is None:
            return None
        with self._lock:
            checkpoints = self._checkpoints(db, checkpoint_ns)
            if not checkpoints:
                return None
            checkpoint_id = get_checkpoint_id(config) or max(checkpoints.keys())
            row = checkpoints.get(checkpoint_id)
            if row is None:
                return None
            return self._tuple_from_row(db, thread_id, checkpoint_ns, checkpoint_id, row)

    def list(
        self,
        config: RunnableConfig | None,
        *,
        filter: dict[str, Any] | None = None,
        before: RunnableConfig | None = None,
        limit: int | None = None,
    ) -> Iterator[CheckpointTuple]:
        thread_ids = (
            [config["configurable"]["thread_id"]] if config else self._threads()
        )
        config_ns = config["configurable"].get("checkpoint_ns") if config else None
        config_id = get_checkpoint_id(config) if config else None
        before_id = get_checkpoint_id(before) if before else None
        remaining = limit
        for thread_id in thread_ids:
            db = self._db(thread_id, create=False)
            if db is None:
                continue
            # Materialize under the lock, yield OUTSIDE it — a generator
            # suspended at yield while holding the lock would block every
            # other thread until the caller finishes iterating.
            batch: list[CheckpointTuple] = []
            with self._lock:
                for checkpoint_ns in self._checkpoint_namespaces(db):
                    if config_ns is not None and checkpoint_ns != config_ns:
                        continue
                    checkpoints = self._checkpoints(db, checkpoint_ns)
                    for checkpoint_id in sorted(checkpoints.keys(), reverse=True):
                        if config_id and checkpoint_id != config_id:
                            continue
                        if before_id and checkpoint_id >= before_id:
                            continue
                        tup = self._tuple_from_row(
                            db, thread_id, checkpoint_ns, checkpoint_id,
                            checkpoints[checkpoint_id],
                        )
                        if filter and not all(
                            tup.metadata.get(k) == v for k, v in filter.items()
                        ):
                            continue
                        batch.append(tup)
            for tup in batch:
                if remaining is not None:
                    if remaining <= 0:
                        return
                    remaining -= 1
                yield tup

    def put(
        self,
        config: RunnableConfig,
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: ChannelVersions,
    ) -> RunnableConfig:
        import json

        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"]["checkpoint_ns"]
        db = self._db(thread_id)
        c = checkpoint.copy()
        values: dict[str, Any] = c.pop("channel_values")  # type: ignore[misc]
        merged = get_checkpoint_metadata(config, metadata)
        ck_type, ck_bytes = self.serde.dumps_typed(c)
        md_type, md_bytes = self.serde.dumps_typed(merged)
        with self._lock:
            # Channel blobs first (idempotent: same bytes = same grain).
            for channel, version in new_versions.items():
                if channel in values:
                    bl_type, bl_bytes = self.serde.dumps_typed(values[channel])
                else:
                    bl_type, bl_bytes = ("empty", b"")
                db.add(
                    "fact",
                    json.dumps(
                        {
                            "subject": f"bl:{enc(checkpoint_ns)}:{enc(channel)}",
                            "relation": f"lg:bl:{version}",
                            "object": b64(bl_bytes),
                            "bl_type": bl_type,
                        }
                    ),
                    ns=_NS,
                )
            # Namespace index (idempotent by content addressing).
            db.add_fact("ix:ns", f"lg:ns:ck-ns:{enc(checkpoint_ns)}", "1", ns=_NS, idempotent=True)
            # The checkpoint grain: ONE identity per id; a re-put of the
            # same id SUPERSEDES (never a sibling), so the tree stays a tree.
            fields = {
                "subject": f"ck:{enc(checkpoint_ns)}",
                "relation": f"lg:ck:{checkpoint['id']}",
                "object": b64(ck_bytes),
                "ck_type": ck_type,
                "md_type": md_type,
                "md_b64": b64(md_bytes),
                "parent_id": config["configurable"].get("checkpoint_id") or "",
            }
            if merged.get("run_id") is not None:
                fields["run_id"] = str(merged["run_id"])
            head = db.latest(fields["subject"], fields["relation"], ns=_NS)
            if head is not None:
                old = json.loads(head)
                if old["fields"].get("object") != fields["object"] or old[
                    "fields"
                ].get("md_b64") != fields["md_b64"]:
                    db.supersede(old["hash"], "fact", json.dumps(fields), ns=_NS)
            else:
                db.add("fact", json.dumps(fields), ns=_NS)
        return {
            "configurable": {
                "thread_id": thread_id,
                "checkpoint_ns": checkpoint_ns,
                "checkpoint_id": checkpoint["id"],
            }
        }

    def put_writes(
        self,
        config: RunnableConfig,
        writes: Sequence[tuple[str, Any]],
        task_id: str,
        task_path: str = "",
    ) -> None:
        import json

        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")
        checkpoint_id = config["configurable"]["checkpoint_id"]
        db = self._db(thread_id)
        with self._lock:
            for idx, (channel, value) in enumerate(writes):
                write_idx = WRITES_IDX_MAP.get(channel, idx)
                subject = f"wr:{enc(checkpoint_ns)}:{checkpoint_id}"
                relation = f"lg:wr:{enc(task_id)}:{write_idx}"
                head = db.latest(subject, relation, ns=_NS)
                if head is not None and write_idx >= 0:
                    continue  # regular writes: first commit wins
                wr_type, wr_bytes = self.serde.dumps_typed(value)
                fields = {
                    "subject": subject,
                    "relation": relation,
                    "object": b64(wr_bytes),
                    "wr_type": wr_type,
                    "channel": channel,
                    "task_id": task_id,
                    "task_path": task_path,
                    "idx": write_idx,
                }
                if head is not None:
                    old = json.loads(head)
                    if old["fields"].get("object") != fields["object"]:
                        # A retry with different bytes SUPERSEDES — never a
                        # second row for the same (task, idx) key.
                        db.supersede(old["hash"], "fact", json.dumps(fields), ns=_NS)
                else:
                    db.add("fact", json.dumps(fields), ns=_NS)

    # ---- DeltaChannel (beta surface; mirrors the reference walk) -----------

    def get_delta_channel_history(
        self, *, config: RunnableConfig, channels: Sequence[str]
    ) -> Mapping[str, DeltaChannelHistory]:
        if not channels:
            return {}
        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")
        checkpoint_id = config["configurable"].get("checkpoint_id", "")
        db = self._db(thread_id, create=False)
        if db is None:
            return {c: {"writes": []} for c in channels}
        with self._lock:
            checkpoints = self._checkpoints(db, checkpoint_ns)

            # The ancestor chain, target's parent first.
            chain: list[str] = []
            target = checkpoints.get(checkpoint_id)
            current = _fields(target).get("parent_id") if target else None
            while current:
                row = checkpoints.get(current)
                if row is None:
                    break
                chain.append(current)
                current = _fields(row).get("parent_id") or None

            collected: dict[str, list[PendingWrite]] = {c: [] for c in channels}
            seed: dict[str, Any] = {}
            remaining = set(channels)
            for cp_id in chain:
                if not remaining:
                    break
                row = checkpoints.get(cp_id)
                f = _fields(row)
                ckpt = self.serde.loads_typed((f["ck_type"], unb64(f["object"])))
                terminated: set[str] = set()
                blob_value: dict[str, Any] = {}
                versions = ckpt.get("channel_versions", {})
                for channel in remaining:
                    version = versions.get(channel)
                    if version is None:
                        continue
                    blob = self._blob(db, checkpoint_ns, channel, version)
                    if blob is None or blob[0] == "empty":
                        continue
                    blob_value[channel] = self.serde.loads_typed(blob)
                    terminated.add(channel)
                step_rows = self._writes(db, checkpoint_ns, cp_id)
                for w in sorted(
                    step_rows,
                    key=lambda r: (_fields(r)["task_id"], int(_fields(r)["idx"])),
                    reverse=True,
                ):
                    wf = _fields(w)
                    if wf["channel"] not in remaining:
                        continue
                    collected[wf["channel"]].append(
                        (
                            wf["task_id"],
                            wf["channel"],
                            self.serde.loads_typed((wf["wr_type"], unb64(wf["object"]))),
                        )
                    )
                for channel in terminated:
                    seed[channel] = blob_value[channel]
                    remaining.discard(channel)

            result: dict[str, DeltaChannelHistory] = {}
            for channel in channels:
                entry: DeltaChannelHistory = {"writes": list(reversed(collected[channel]))}
                if channel in seed:
                    entry["seed"] = seed[channel]
                result[channel] = entry
            return result

    # ---- lifecycle ---------------------------------------------------------

    def delete_thread(self, thread_id: str) -> None:
        """Erase one thread: close the handle and delete its FILE — the
        memory file is the isolation unit, so this is complete erasure of
        the thread's checkpoints, writes, blobs, and history."""
        with self._lock:
            handle = self._handles.pop(thread_id, None)
            del handle
            path = self._path(thread_id)
            self._delete_files(path)

    @staticmethod
    def _delete_files(path: str) -> None:
        for suffix in ("", "-wal", ".blobs", ".telemetry.db", ".telemetry.db-wal"):
            p = path + suffix
            if os.path.isdir(p):
                import shutil

                shutil.rmtree(p, ignore_errors=True)
            elif os.path.exists(p):
                os.remove(p)

    def delete_for_runs(self, run_ids: Sequence[str]) -> None:
        wanted = {str(r) for r in run_ids}
        for thread_id in self._threads():
            db = self._db(thread_id)
            with self._lock:
                for checkpoint_ns in self._checkpoint_namespaces(db):
                    checkpoints = self._checkpoints(db, checkpoint_ns)
                    victims = {
                        cid: row
                        for cid, row in checkpoints.items()
                        if _fields(row).get("run_id") in wanted
                    }
                    if not victims:
                        continue
                    # Deleting a run must delete its DATA: forget channel
                    # blobs the dying checkpoints reference, unless a
                    # SURVIVING checkpoint still references the same
                    # (channel, version).
                    surviving_refs = set()
                    for cid, row in checkpoints.items():
                        if cid in victims:
                            continue
                        f = _fields(row)
                        ckpt = self.serde.loads_typed((f["ck_type"], unb64(f["object"])))
                        for ch, v in ckpt.get("channel_versions", {}).items():
                            surviving_refs.add((ch, str(v)))
                    for cid, row in victims.items():
                        f = _fields(row)
                        ckpt = self.serde.loads_typed((f["ck_type"], unb64(f["object"])))
                        for ch, v in ckpt.get("channel_versions", {}).items():
                            if (ch, str(v)) in surviving_refs:
                                continue
                            head = db.latest(
                                f"bl:{enc(checkpoint_ns)}:{enc(ch)}", f"lg:bl:{v}", ns=_NS
                            )
                            if head is not None:
                                import json as _json

                                db.forget(_json.loads(head)["hash"])
                        self._forget_checkpoint(db, checkpoint_ns, cid, row)

    def _checkpoint_namespaces(self, db: areev.Areev) -> list[str]:
        return [
            dec(raw[len("ck-ns:") :])
            for raw in self._namespaces(db)
            if raw.startswith("ck-ns:")
        ]

    def _forget_checkpoint(
        self, db: areev.Areev, checkpoint_ns: str, checkpoint_id: str, row: dict
    ) -> None:
        for w in self._writes(db, checkpoint_ns, checkpoint_id):
            db.forget(w["hash"])
        db.forget(row["hash"])

    def copy_thread(self, source_thread_id: str, target_thread_id: str) -> None:
        """Copy the COMPLETE chain (checkpoints, writes, blobs) — a partial
        copy would leave DeltaChannel state that cannot reconstruct.
        Payload grains are content-addressed, so identical values dedup."""
        import json

        src = self._db(source_thread_id, create=False)
        if src is None:
            return
        dst = self._db(target_thread_id)
        with self._lock:
            for subject_rows in self._all_rows(src):
                for row in subject_rows:
                    f = dict(_fields(row))
                    f.pop("namespace", None)
                    f.pop("type", None)
                    dst.add("fact", json.dumps(f), ns=_NS)

    def _all_rows(self, db: areev.Areev) -> Iterator[list[dict]]:
        yield self._recall(db, "ix:ns")
        for checkpoint_ns in self._checkpoint_namespaces(db):
            checkpoints = self._checkpoints(db, checkpoint_ns)
            yield list(checkpoints.values())
            for checkpoint_id, row in checkpoints.items():
                yield self._writes(db, checkpoint_ns, checkpoint_id)
                f = _fields(row)
                ckpt = self.serde.loads_typed((f["ck_type"], unb64(f["object"])))
                for channel, version in ckpt.get("channel_versions", {}).items():
                    head = db.latest(
                        f"bl:{enc(checkpoint_ns)}:{enc(channel)}",
                        f"lg:bl:{version}",
                        ns=_NS,
                    )
                    if head is not None:
                        import json as _json

                        yield [_json.loads(head)]

    def prune(self, thread_ids: Sequence[str], *, strategy: str = "keep_latest") -> None:
        """`keep_latest` keeps the newest checkpoint per namespace AND its
        full ancestor chain (with their writes) — DeltaChannel
        reconstruction walks that chain, so pruning intermediates would
        silently sever it (the base-class warning); only side branches are
        removed. `delete` removes the thread files entirely."""
        if strategy == "delete":
            for thread_id in thread_ids:
                self.delete_thread(thread_id)
            return
        if strategy != "keep_latest":
            raise ValueError(f"unknown prune strategy {strategy!r}")
        for thread_id in thread_ids:
            db = self._db(thread_id, create=False)
            if db is None:
                continue
            with self._lock:
                for checkpoint_ns in self._checkpoint_namespaces(db):
                    checkpoints = self._checkpoints(db, checkpoint_ns)
                    if not checkpoints:
                        continue
                    keep: set[str] = set()
                    current: str | None = max(checkpoints.keys())
                    while current and current in checkpoints:
                        keep.add(current)
                        current = _fields(checkpoints[current]).get("parent_id") or None
                    for checkpoint_id, row in checkpoints.items():
                        if checkpoint_id not in keep:
                            self._forget_checkpoint(db, checkpoint_ns, checkpoint_id, row)

    def get_next_version(self, current: str | None, channel: None) -> str:
        """32-digit zero-padded integer prefix (lexicographic == numeric,
        property-tested) + a RANDOM tail. The tail is load-bearing, exactly
        as in the reference saver: two sibling branches forked from one
        checkpoint both mint v+1 for the same channel, and blobs are keyed
        (channel, version) — with a fixed tail both branches would write
        one blob cell and silently read each other's values. LangGraph's
        checkpointer has no replay contract of ours, so the randomness
        breaks nothing."""
        import os as _os

        if current is None:
            current_v = 0
        elif isinstance(current, int):
            current_v = current
        else:
            current_v = int(current.split(".")[0])
        tail = int.from_bytes(_os.urandom(8), "big")
        return f"{current_v + 1:032}.{tail:020}"

    # ---- async twins (storage calls release the GIL; plain delegation) ----

    async def aget_tuple(self, config: RunnableConfig) -> CheckpointTuple | None:
        return self.get_tuple(config)

    async def alist(
        self,
        config: RunnableConfig | None,
        *,
        filter: dict[str, Any] | None = None,
        before: RunnableConfig | None = None,
        limit: int | None = None,
    ) -> AsyncIterator[CheckpointTuple]:
        for item in self.list(config, filter=filter, before=before, limit=limit):
            yield item

    async def aput(
        self,
        config: RunnableConfig,
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: ChannelVersions,
    ) -> RunnableConfig:
        return self.put(config, checkpoint, metadata, new_versions)

    async def aput_writes(
        self,
        config: RunnableConfig,
        writes: Sequence[tuple[str, Any]],
        task_id: str,
        task_path: str = "",
    ) -> None:
        return self.put_writes(config, writes, task_id, task_path)

    async def adelete_thread(self, thread_id: str) -> None:
        return self.delete_thread(thread_id)

    async def adelete_for_runs(self, run_ids: Sequence[str]) -> None:
        return self.delete_for_runs(run_ids)

    async def acopy_thread(self, source_thread_id: str, target_thread_id: str) -> None:
        return self.copy_thread(source_thread_id, target_thread_id)

    async def aprune(
        self, thread_ids: Sequence[str], *, strategy: str = "keep_latest"
    ) -> None:
        return self.prune(thread_ids, strategy=strategy)

    async def aget_delta_channel_history(
        self, *, config: RunnableConfig, channels: Sequence[str]
    ) -> Mapping[str, DeltaChannelHistory]:
        return self.get_delta_channel_history(config=config, channels=channels)
