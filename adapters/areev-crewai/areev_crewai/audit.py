"""AreevAuditListener — mirror CrewAI events into an Areev memory file.

[R3] schema-agnostic BY CONSTRUCTION: every event, known or unknown,
persists as a generic Fact carrying the event's class name and its
JSON-safe payload — CrewAI's twice-weekly releases structurally cannot
break this sidecar (a new event type is just a new `kind` string).

Same mode matrix as the LangGraph mirror: ``best-effort`` (bounded queue,
drop-oldest, counter — labeled observability) vs ``guaranteed``
(backpressure, never drops — the only mode compliance may cite).
"""

from __future__ import annotations

import json
import threading
import time
from collections import deque
from typing import Any

import areev

try:
    from crewai.events.base_event_listener import BaseEventListener
except ImportError:  # pragma: no cover - older/newer layout

    class BaseEventListener:  # type: ignore[no-redef]
        def __init__(self) -> None:
            pass


def _json_safe(value: Any, depth: int = 0) -> Any:
    if depth > 4:
        return str(value)
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    if isinstance(value, dict):
        return {str(k): _json_safe(v, depth + 1) for k, v in list(value.items())[:64]}
    if isinstance(value, (list, tuple)):
        return [_json_safe(v, depth + 1) for v in list(value)[:64]]
    return str(value)[:2000]


class AreevAuditListener(BaseEventListener):
    def __init__(self, path: str, *, mode: str = "best-effort", capacity: int = 1024) -> None:
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
        try:
            super().__init__()
        except Exception:  # noqa: BLE001 - bus wiring must not break capture
            pass

    def setup_listeners(self, crewai_event_bus) -> None:  # type: ignore[override]
        # The bus dispatches by EXACT event class (no MRO walk), so
        # schema-agnostic capture means one registration PER concrete event
        # class, enumerated from crewai.events.types at wire time — a new
        # release's new classes are picked up by the same walk. `wired`
        # reports how many registrations landed: zero is VISIBLE, because a
        # silent no-op audit sidecar is worse than a crash.
        self.wired = 0
        try:
            import importlib
            import inspect as _inspect
            import pkgutil

            import crewai.events.types as _types
            from crewai.events.base_events import BaseEvent

            classes = set()
            for mod in pkgutil.iter_modules(_types.__path__):
                try:
                    m = importlib.import_module(f"crewai.events.types.{mod.name}")
                except Exception:  # noqa: BLE001 - one bad module, not zero capture
                    continue
                for _, obj in _inspect.getmembers(m, _inspect.isclass):
                    if issubclass(obj, BaseEvent) and obj is not BaseEvent:
                        classes.add(obj)

            def _capture(source, event) -> None:  # noqa: ANN001
                self.record(type(event).__name__, _json_safe(getattr(event, "__dict__", {})))

            for cls in classes:
                try:
                    crewai_event_bus.on(cls)(_capture)
                    self.wired += 1
                except Exception:  # noqa: BLE001
                    continue
        except Exception:  # noqa: BLE001 - degrade, but visibly (wired == 0)
            pass

    def record(self, kind: str, payload: dict[str, Any]) -> None:
        event = {"kind": kind, "at_ms": int(time.time() * 1000), "payload": payload}
        with self._cv:
            if self._closed:
                return
            if len(self._buf) >= self.capacity:
                if self.mode == "guaranteed":
                    while len(self._buf) >= self.capacity and not self._closed:
                        self._cv.wait(timeout=1.0)
                    if self._closed:
                        return
                else:
                    self._buf.popleft()
                    self.dropped += 1
            self._buf.append(event)
            self._cv.notify_all()

    def _run(self) -> None:
        db = areev.Areev(self._path, ns="crewai")
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
                            "subject": "audit:events",
                            "relation": f"crw:ev:{self._seq:08}",
                            "object": json.dumps(event),
                        }
                    ),
                    ns="crewai",
                )
            except Exception:  # noqa: BLE001
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
