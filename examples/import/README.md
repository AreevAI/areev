# Import a tool-call log → cluster failures from history

If you have tool-call logs that predate Areev, import them as Tool grains so
the flagship analyzer can cluster failures from day one — no need to wait for
new sessions.

[`tool-calls.jsonl`](tool-calls.jsonl) is a small OpenAI-style log: one JSON
object per line. The importer accepts a direct `{tool_name, content,
is_error}` record, an OpenAI `role: "tool"` result, or an assistant message
carrying a `tool_calls` array.

```bash
areev migrate --from tool-log --file examples/import/tool-calls.jsonl --db agent.db --ns caller
#   {"added": 7, ...}

areev loop run --db agent.db --ns caller
areev loop list --db agent.db --ns caller
#   … Tool "stripe_refund" failed 3 times (75% of calls): # rate_limited: too many requests
#   (digit runs collapse to '#' so the signature is stable across error codes)

# review with judgment — apply the lesson, or dismiss it with a reason
areev loop approve <hash> --db agent.db --ns caller --because "retries belong in the client"
areev loop apply   <hash> --db agent.db --ns caller --because "codifying the retry rule"
```

`stripe_refund` failed 3 of its 4 calls (75% ≥ the 40% threshold, 3 ≥ the
minimum cluster size), so it clusters into one lesson. `send_email` failed
once — below threshold, correctly ignored.
