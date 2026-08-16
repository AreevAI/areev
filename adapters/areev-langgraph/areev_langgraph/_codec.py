"""Shared encoding for the LangGraph adapters.

Every LangGraph identifier that becomes part of an Areev subject or
relation is percent-encoded with NO safe characters, so component
boundaries (`:`) can never be forged by identifier content — `("a", "b:c")`
and `("a:b", "c")` stay distinct. The encoding is reversible by
construction (property-tested in tests/test_properties.py).

Binary payloads (the checkpointer's typed-serialized bytes) ride as
base64 in Fact objects; the serializer's type tag rides as an extra field
so `loads_typed` reconstructs exactly what `dumps_typed` produced.
"""

from __future__ import annotations

import base64
from urllib.parse import quote, unquote


def enc(component: str) -> str:
    """Percent-encode one identifier component (no safe characters).

    The EMPTY component encodes as a bare ``%`` — `quote` only ever emits
    ``%XX`` pairs, so no real component can collide with it; without the
    sentinel, ``("",)`` and ``()`` would both map to ``""``.
    """
    if component == "":
        return "%"
    return quote(component, safe="")


def dec(component: str) -> str:
    if component == "%":
        return ""
    return unquote(component)


def b64(data: bytes) -> str:
    # Facts refuse empty objects (VAL-E001); "-" is the empty-payload
    # sentinel — never valid base64, so the mapping stays unambiguous.
    if not data:
        return "-"
    return base64.b64encode(data).decode("ascii")


def unb64(text: str) -> bytes:
    if text == "-":
        return b""
    return base64.b64decode(text.encode("ascii"))


def ns_to_str(namespace: tuple[str, ...]) -> str:
    """A LangGraph namespace tuple as one reversible path string."""
    return "/".join(enc(part) for part in namespace)


def str_to_ns(path: str) -> tuple[str, ...]:
    if path == "":
        return ()
    return tuple(dec(part) for part in path.split("/"))
