#!/usr/bin/env python3
"""Precompute OpenAI embeddings for every LoCoMo turn + question and write a
{text: vector} cache JSON that the Rust `accuracy` bin loads via
$AREEV_EMBED_CACHE (its CachedEmbed backend looks up by exact text).

    usage:  embed_locomo.py <locomo10.json> <out_cache.json> [dims] [conv_limit]
            dims default 512 (text-embedding-3-small supports 256..1536)
    key:    $OPENAI_API_KEY, else /tmp/areev_openai_key, else ~/.areev_openai_key

Turn text MUST match the Rust harness exactly: "{speaker}: {text}".
Stdlib only (urllib) — no pip install.
"""
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

MODEL = "text-embedding-3-small"
ENDPOINT = "https://api.openai.com/v1/embeddings"


def get_key() -> str:
    k = os.environ.get("OPENAI_API_KEY")
    if k:
        return k.strip()
    for p in ("/tmp/areev_openai_key", os.path.expanduser("~/.areev_openai_key")):
        if os.path.exists(p):
            return open(p).read().strip()
    sys.exit("no OpenAI key: set $OPENAI_API_KEY or write /tmp/areev_openai_key")


def embed_batch(texts, dims, key):
    body = json.dumps({"model": MODEL, "input": texts, "dimensions": dims}).encode()
    req = urllib.request.Request(
        ENDPOINT,
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                out = json.load(r)
            return [d["embedding"] for d in out["data"]]
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503) and attempt < 5:
                time.sleep(2 ** attempt)
                continue
            raise
        except Exception:  # noqa: BLE001
            if attempt < 5:
                time.sleep(2 ** attempt)
                continue
            raise


def main() -> None:
    if len(sys.argv) < 3:
        sys.exit("usage: embed_locomo.py <locomo10.json> <out_cache.json> [dims] [conv_limit]")
    path, outp = sys.argv[1], sys.argv[2]
    dims = int(sys.argv[3]) if len(sys.argv) > 3 else 512
    limit = int(sys.argv[4]) if len(sys.argv) > 4 else 10**9
    key = get_key()

    data = json.load(open(path))[:limit]
    texts = set()
    for s in data:
        for k, v in s.get("conversation", {}).items():
            if re.fullmatch(r"session_\d+", k) and isinstance(v, list):
                for t in v:
                    txt = f'{t.get("speaker", "")}: {t.get("text", "")}'
                    if t.get("text"):
                        texts.add(txt)
        for q in s.get("qa", []):
            if q.get("question"):
                texts.add(q["question"])
    texts = sorted(texts)
    print(f"embedding {len(texts)} unique texts at {dims}-d via {MODEL}", file=sys.stderr)

    cache = {}
    B = 1000
    for i in range(0, len(texts), B):
        chunk = texts[i : i + B]
        for t, vec in zip(chunk, embed_batch(chunk, dims, key)):
            cache[t] = vec
        print(f"  {min(i + B, len(texts))}/{len(texts)}", file=sys.stderr)
    json.dump(cache, open(outp, "w"))
    print(f"wrote {len(cache)} embeddings ({dims}-d) to {outp}")


if __name__ == "__main__":
    main()
