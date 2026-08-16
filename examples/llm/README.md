# Areev Loop LLM enrichment backends (`--llm-cmd`)

The optional LLM layer (proposal §9) is a **subprocess protocol**, exactly like
`--embed-cmd`: the loop writes one JSON request to the command's stdin and reads
one JSON response from its stdout. No SDK, no network code in Areev.

```bash
areev loop run --db agent.db --llm-cmd './examples/llm/claude.sh'
```

The LLM can only **add** to the deterministic output — it never gates or
rewrites it:

- **DISCOVER** — propose *additional* draft recommendations. Every draft is
  stamped `origin = llm` (so it can **never auto-apply**), must **cite evidence
  hashes** present in the request bundle (uncited drafts are dropped), and must
  target a memory entity. LLM drafts are advisory flags for a human to review.
- **ENRICH** — add a one-line `guidance` note to a deterministic finding. The
  engine-templated summary is always kept.

A failed, slow, or garbled backend drops the LLM contribution for that run — it
never fails the run.

## Protocol

**Request** (stdin), one JSON object:

```json
{
  "loop": 1,
  "op": "probe" | "discover" | "ground" | "verify" | "enrich",
  "instructions": "<fixed engine instruction — treat as the system prompt>",
  "findings":  [{"analyzer": "...", "summary": "...", "target": "...", "severity": "..."}],
  "evidence":  [{"hash": "...", "grain_type": "...", "text": "..."}],
  "claims":    [{"id": 0, "claim": "...", "evidence": [{"hash","text"}]}],
  "rejected":  ["<recent operator rejections>"],
  "approved":  ["<recent operator approvals>"]
}
```

`instructions` is kept in its own field and never interleaved with (possibly
attacker-influenced) `evidence` text — keep it that way in your prompt.

**Response** (stdout), one JSON object:

| op         | response                                                                 |
|------------|--------------------------------------------------------------------------|
| `probe`    | `{"model": "<name>"}`                                                     |
| `discover` | `{"recommendations": [{"summary","target","guidance","evidence":[hash],"confidence":0.0}]}`|
| `ground`   | `{"results": [{"id":0,"supported":true,"reason":"..."}]}`                 |
| `verify`   | `{"results": [{"id":0,"keep":true,"confidence":0.0,"reason":"..."}]}`     |
| `enrich`   | `{"notes": [{"target","guidance"}]}`                                      |

The pipeline is `DISCOVER → GROUND → VERIFY → ENRICH`, each a **separate call**
(the proposer never grades itself — the anti-Goodhart rule):

- **`ground`** — for each `claims[]` entry, decide whether its cited evidence
  *entails* the claim (decompose-then-entail; be conservative). A draft that
  isn't `supported` is dropped before verification.
- **`verify`** — for each grounded finding, adversarially try to reject it
  (novel? real? in-context?), return `keep` + a calibrated `confidence`; default
  to `keep:false` when uncertain. Only drafts kept above the confidence floor
  reach the review queue.

Return **only** JSON. Unknown fields are dropped; strings are capped; a response
that doesn't parse drops that stage's contribution (safe default). See
`docs/loop-reflection.md` for the full design.

## Backends here

- `claude.sh` — the Claude Code CLI (`claude -p`). Needs `claude` and `jq`.
- `openai.py` — ~15 lines over the OpenAI API. Needs `OPENAI_API_KEY`.
- `ollama.sh` — a local model via `ollama`. Needs `ollama` and `jq`.

All three answer `probe` locally (no model call) and shell the model only for
`discover`/`enrich`.
