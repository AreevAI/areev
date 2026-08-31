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
  stamped `origin = llm` (so it can **never auto-apply**) and must **cite
  evidence hashes** present in the request bundle (uncited drafts are dropped).
  A draft with no `proposal` is an advisory flag for a human to read. A draft
  that carries one is asking for a specific change, from a closed vocabulary
  of five kinds:

  ```jsonc
  "proposal": {"kind": "lesson",          "lesson": "one imperative line"}
  "proposal": {"kind": "fact",            "relation": "alias_of", "object": "…"}
  "proposal": {"kind": "query_revision",  "body": "<the full new CAL body>"}
  "proposal": {"kind": "plan_revision",   "edits": [{"path": "retries.fetch",
                                                     "from": 1, "to": 3}]}
  "proposal": {"kind": "code_revision",   "source": "<the full new source>"}
  ```

  The **scope always comes from `target`**, never from the proposal: the
  subject of a fact, the name of a query, the plan hash, the tool name — and
  for a code revision the evalset it will be graded against, which is read off
  the tool's own definition. A backend cannot choose what its change applies
  to, or which gate judges it. Unknown or malformed kinds leave the draft
  advisory rather than dropping the response.

  Every kind still needs a human review with a written reason and an explicit
  apply. `query_revision` additionally requires the substrate to record a
  rollback inverse or the apply is refused; `plan_revision` needs the `plans`
  capability and cannot express node topology; `code_revision` needs the
  `code` capability and applies only through a recorded eval run (Rule E1).
  Full contract: [`docs/loop.md`](../../docs/loop.md), "LLM enrichment".
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

- `mock.py` — deterministic, no model, no network, no key. Two modes:

  ```bash
  # 1. default: one advisory draft citing the first bundled evidence hash
  areev loop run --db agent.db --llm-cmd 'python3 examples/llm/mock.py'

  # 2. fixture-driven: replay an exact proposal you committed
  AREEV_MOCK_LLM_FIXTURE=path/to/draft.json \
    areev loop run --db agent.db --llm-cmd 'python3 examples/llm/mock.py'
  ```

  The fixture holds a DISCOVER response verbatim. Any draft without an
  `evidence` list gets the first bundled hash filled in — the hashes are
  content addresses that do not exist until the memory has been written, so a
  fixture cannot know them and a hard-coded one would rot on any change to
  the data it came from.

  This is how [`agents/rcm-optimization`](../agents/rcm-optimization/) and
  [`agents/sanctions-screening`](../agents/sanctions-screening/) exercise the
  whole governed path — propose, ground, verify, review, apply, roll back —
  in CI with no key. **It is never a learning claim**: it proves the path
  exists, not that a model would propose anything.
