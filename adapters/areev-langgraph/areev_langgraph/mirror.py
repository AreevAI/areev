"""AreevTraceMirror — mirror LangGraph/LangChain callback events into a
Areev memory file.

Surface pinned per the [R3] review: a SYNC `BaseCallbackHandler` attached
via config; the app thread only ENQUEUES (microseconds, never a store
write); ONE worker thread owns the Areev handle — the single-writer rule
is satisfied by construction.

Mode matrix (the Art. 12 honesty rule):

- ``best-effort`` (default): bounded queue, drop-oldest with a counter —
  labeled *observability*. Under load it sheds events and SAYS so.
- ``guaranteed``: the enqueue BLOCKS when the queue is full
  (backpressure into the app). Compliance/forensics positioning may cite
  only this mode — a mirror that sheds audit events under load cannot
  back a lifecycle-logging pitch, counter or no counter.

Events land as Facts: subject ``trace:<run_id>``, relation
``lg:ev:<seq>``, object = the JSON event payload — queryable with plain
recall/CAL, replicating with the file.
"""

from __future__ import annotations

import json
import threading
import time
from collections import deque
from typing import Any

import areev

try:  # langchain-core ships with langgraph; standalone installs may lack it
    from langchain_core.callbacks import BaseCallbackHandler
except ImportError:  # pragma: no cover

    class BaseCallbackHandler:  # type: ignore[no-redef]
        pass


class AreevTraceMirror(BaseCallbackHandler):
    def __init__(
        self,
        path: str,
        *,
        mode: str = "best-effort",
        capacity: int = 1024,
    ) -> None:
        if mode not in ("best-effort", "guaranteed"):
            raise ValueError("mode must be 'best-effort' or 'guaranteed'")
        self.mode = mode
        self.capacity = capacity
        self.dropped = 0
        self._seq = 0
        self._buf: deque[dict] = deque()
        self._cv = threading.Condition()
        self._closed = False
        self._inflight = False
        self._path = path
        self._worker = threading.Thread(target=self._run, daemon=True)
        self._worker.start()

    # ---- enqueue (app thread; never touches the store) ---------------------

    def _emit(self, kind: str, run_id: Any, payload: dict[str, Any]) -> None:
        event = {"kind": kind, "run_id": str(run_id), "at_ms": int(time.time() * 1000), **payload}
        with self._cv:
            if self._closed:
                return
            if len(self._buf) >= self.capacity:
                if self.mode == "guaranteed":
                    # Backpressure: never drop; wait for the worker.
                    while len(self._buf) >= self.capacity and not self._closed:
                        self._cv.wait(timeout=1.0)
                    if self._closed:
                        return
                else:
                    self._buf.popleft()
                    self.dropped += 1
            self._buf.append(event)
            self._cv.notify_all()

    # ---- worker (owns the handle) ------------------------------------------

    def _run(self) -> None:
        db = areev.Areev(self._path, ns="langgraph")
        while True:
            with self._cv:
                while not self._buf and not self._closed:
                    self._cv.wait()
                if not self._buf and self._closed:
                    return
                event = self._buf.popleft()
                self._inflight = True
                self._cv.notify_all()
            self._seq += 1
            try:
                db.add(
                    "fact",
                    json.dumps(
                        {
                            "subject": f"trace:{event['run_id']}",
                            "relation": f"lg:ev:{self._seq:08}",
                            "object": json.dumps(event),
                        }
                    ),
                    ns="langgraph",
                )
            except Exception:  # noqa: BLE001 - a mirror must never kill the app
                self.dropped += 1
            with self._cv:
                self._inflight = False
                self._cv.notify_all()

    def flush(self, timeout: float = 10.0) -> bool:
        """Wait until every queued event — including the one mid-write — is
        durably stored. Returns False on timeout (partial flush), never
        silently."""
        deadline = time.time() + timeout
        with self._cv:
            while (self._buf or self._inflight) and time.time() < deadline:
                self._cv.wait(timeout=0.1)
            return not self._buf and not self._inflight

    def close(self) -> None:
        with self._cv:
            self._closed = True
            self._cv.notify_all()
        if self.mode == "guaranteed":
            # Guaranteed mode never abandons queued events: join without a
            # deadline (the worker exits once the buffer drains).
            self._worker.join()
        else:
            self._worker.join(timeout=10.0)

    # ---- the callback surface (schema-light on purpose) --------------------

    def on_chain_start(self, serialized, inputs, *, run_id, **kwargs):  # type: ignore[override]
        name = (serialized or {}).get("name") if isinstance(serialized, dict) else None
        self._emit("chain_start", run_id, {"name": name})

    def on_chain_end(self, outputs, *, run_id, **kwargs):  # type: ignore[override]
        self._emit("chain_end", run_id, {})

    def on_chain_error(self, error, *, run_id, **kwargs):  # type: ignore[override]
        self._emit("chain_error", run_id, {"error": str(error)})

    def on_llm_start(self, serialized, prompts, *, run_id, **kwargs):  # type: ignore[override]
        self._emit("llm_start", run_id, {"prompts": len(prompts)})

    def on_llm_end(self, response, *, run_id, **kwargs):  # type: ignore[override]
        usage = None
        try:
            usage = response.llm_output.get("token_usage")  # type: ignore[union-attr]
        except Exception:  # noqa: BLE001
            pass
        self._emit("llm_end", run_id, {"usage": usage})

    def on_llm_error(self, error, *, run_id, **kwargs):  # type: ignore[override]
        self._emit("llm_error", run_id, {"error": str(error)})

    def on_tool_start(self, serialized, input_str, *, run_id, **kwargs):  # type: ignore[override]
        name = (serialized or {}).get("name") if isinstance(serialized, dict) else None
        self._emit("tool_start", run_id, {"name": name})

    def on_tool_end(self, output, *, run_id, **kwargs):  # type: ignore[override]
        self._emit("tool_end", run_id, {})

    def on_tool_error(self, error, *, run_id, **kwargs):  # type: ignore[override]
        self._emit("tool_error", run_id, {"error": str(error)})
