// End-to-end smoke test for the `areev` napi-rs binding.
//
// Drives the real compiled native addon against a fresh temp database. The
// FFI convention is "scalars in, JSON strings out", so every structured return
// is JSON.parse'd and asserted on shape + content — mirroring
// crates/areev-py/tests/test_areev.py. Run: `node --test __test__`.
//
// Every method returns a promise, so every call here is awaited. Rejections
// are asserted with `assert.rejects`, not `assert.throws`: a promise-returning
// method reports a bad argument by rejecting, and `assert.throws` would pass
// vacuously against an unhandled rejection.

import test from 'node:test'
import assert from 'node:assert/strict'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'

import { Areev } from '../index.js'

const HEX64 = 64 // length of a SHA-256 content address in hex

function makeDb(ns = 'caller') {
  const dir = mkdtempSync(join(tmpdir(), 'areev-js-'))
  return new Areev(join(dir, 'test.db'), ns)
}

test('module exposes the Areev class', () => {
  assert.equal(typeof Areev, 'function')
})

test('every store method returns a promise', async () => {
  const m = makeDb()
  const p = m.addFact('john', 'prefers', 'tea')
  assert.ok(p instanceof Promise, 'addFact must return a promise')
  await p
  assert.ok(m.recall('john') instanceof Promise, 'recall must return a promise')
  assert.ok(m.stats() instanceof Promise, 'stats must return a promise')
})

test('a store call does not block the event loop', async () => {
  // The reason this surface is async at all. A timer scheduled before a long
  // call must still fire while that call is in flight; with the old
  // synchronous methods it could not run until the call returned.
  const m = makeDb('bulk')
  const rows = Array.from({ length: 400 }, (_, i) =>
    JSON.stringify({ subject: `user${i % 20}`, relation: 'prefers', object: `value ${i}` }),
  ).join('\n')

  let tickedDuringCall = false
  const timer = setInterval(() => {
    tickedDuringCall = true
  }, 5)

  const report = JSON.parse(await m.migrate('jsonl', rows, null, 'bulk'))
  clearInterval(timer)

  assert.equal(report.added, 400)
  assert.ok(tickedDuringCall, 'a timer must fire while a store call is in flight')
})

test('addFact returns a 64-hex content address', async () => {
  const m = makeDb()
  const h = await m.addFact('john', 'prefers', 'tea', 0.95)
  assert.equal(typeof h, 'string')
  assert.equal(h.length, HEX64)
  assert.match(h, /^[0-9a-f]{64}$/)
})

test('recall roundtrip parses and carries the fields', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')

  const rows = JSON.parse(await m.recall('john'))
  assert.ok(Array.isArray(rows))
  assert.equal(rows.length, 1)

  const row = rows[0]
  for (const key of ['hash', 'type', 'fields']) assert.ok(key in row)
  assert.equal(row.type, 'fact')
  assert.equal(row.hash.length, HEX64)
  assert.equal(row.fields.subject, 'john')
  assert.equal(row.fields.relation, 'prefers')
  assert.equal(row.fields.object, 'tea')
})

test('recall relation filter narrows results', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  await m.addFact('john', 'speaks', 'german')

  assert.equal(JSON.parse(await m.recall('john')).length, 2)

  const speaks = JSON.parse(await m.recall('john', 'speaks'))
  assert.equal(speaks.length, 1)
  assert.equal(speaks[0].fields.object, 'german')
})

test('add() generic grain from JSON fields', async () => {
  const m = makeDb()
  const h = await m.add(
    'fact',
    JSON.stringify({ subject: 'alice', relation: 'likes', object: 'coffee', confidence: 0.8 }),
  )
  assert.equal(h.length, HEX64)
  const rows = JSON.parse(await m.recall('alice'))
  assert.equal(rows[0].fields.object, 'coffee')
})

test('cal RECALL returns the grains wire payload', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')

  const payload = JSON.parse(await m.cal('RECALL facts WHERE subject = "john"'))
  assert.equal(payload.type, 'grains')
  assert.ok(Array.isArray(payload.grains))
  assert.equal(payload.grains.length, 1)

  const grain = payload.grains[0]
  assert.equal(grain.grain_type, 'fact')
  assert.equal(grain.fields.object, 'tea')
  assert.equal(grain.hash.length, HEX64)
})

test('cal surfaces CAL-W* warnings instead of dropping them', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')

  // `score_breakdown` cannot change a RECALL result, and the executor says so
  // with CAL-W014. The binding used to serialize the payload alone, so the
  // warning never left the process and the option read as a silent no-op (#68).
  const withOpt = JSON.parse(await m.cal('RECALL facts WHERE subject = "john" WITH score_breakdown'))
  assert.ok(Array.isArray(withOpt.warnings), 'warnings must reach the caller')
  assert.ok(withOpt.warnings.some((w) => w.includes('CAL-W014')), 'CAL-W014 must be named')
  assert.equal(withOpt.type, 'grains', 'the rest of the payload is unchanged')

  // A clean query keeps the exact shape it always had.
  const clean = JSON.parse(await m.cal('RECALL facts WHERE subject = "john"'))
  assert.equal(clean.warnings, undefined, 'no warnings means no key')
})

test('recordToolCall records occurrences, not values', async () => {
  const m = makeDb()
  // Byte-identical retries must each survive as their own grain, or the
  // tool-failure analyzer has nothing to count (#66).
  for (let i = 0; i < 3; i++) await m.recordToolCall('stripe_refund', 'rate_limited', true)
  const n = JSON.parse(await m.cal('RECALL tools WHERE tool_name = "stripe_refund" | COUNT'))
  assert.equal(n.count, 3, 'three retries are three occurrences')

  // A host-supplied call id lands on the grain and is filterable.
  await m.recordToolCall('stripe_refund', 'rate_limited', true, null, 'call_xyz', '{"amount":42}')
  const byId = JSON.parse(await m.cal('RECALL tools WHERE tool_call_id = "call_xyz"'))
  assert.equal(byId.grains.length, 1)
  assert.equal(byId.grains[0].fields.input.amount, 42)

  const manifest = JSON.parse(await m.recordRunManifest(
    'run-js', '{"model":{"base":"test"},"sampling":{"seed":7}}'))
  assert.equal(manifest.config_hash.length, 64)
  assert.equal(manifest.link_hash.length, 64)
  m.setRunId('run-js')
  await m.recall('acct')
  m.setRunId(null)
})

test('cal COUNT pipeline', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  await m.addFact('john', 'speaks', 'german')

  const payload = JSON.parse(await m.cal('RECALL facts WHERE subject = "john" | COUNT'))
  assert.equal(payload.type, 'count')
  assert.equal(payload.count, 2)
})

test('stats() returns a parseable JSON object', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  const s = JSON.parse(await m.stats())
  assert.equal(typeof s.grains, 'number')
  assert.ok(s.grains >= 1)
})

test('bad input rejects with a JS Error', async () => {
  const m = makeDb()
  // CAL structurally has no DELETE token -> parse/exec error surfaces as Error.
  await assert.rejects(() => m.cal('DELETE sha256:abc'), Error)
  // Malformed JSON fields for add() -> Error.
  await assert.rejects(() => m.add('fact', 'not-json'), Error)
  // Invalid content address for forget() -> Error.
  await assert.rejects(() => m.forget('nothex'), Error)
})

// A scripted fake speaking the Areev Loop wire protocol — hermetic, no network
// and no key, the same shape crates/areev-cli/tests/remember_llm_tests.rs uses.
const FAKE_LLM_PY = `
import json, sys
d = json.loads(sys.stdin.read())
op = d.get("op", "")
if op == "probe":
    print(json.dumps({"model": "fake-extractor-1"}))
elif op == "extract":
    print(json.dumps({"facts": [
        {"subject": "john", "relation": "prefers", "object": "window seat", "confidence": 0.9},
        {"subject": "john", "relation": "guess", "object": "likes jazz", "confidence": 0.2},
    ]}))
elif op == "ground":
    print(json.dumps({"results": [
        {"id": c["id"], "supported": "guess" not in c["claim"], "reason": "checked"}
        for c in d.get("claims", [])
    ]}))
else:
    print(json.dumps({}))
`

/// Write the fake to a temp file and return its `llmCmd` string, or null when
/// there is no python to run it with (skip rather than fail).
function fakeLlm() {
  for (const py of ['python3', 'python']) {
    const probe = spawnSync(py, ['--version'])
    if (probe.status === 0) {
      const dir = mkdtempSync(join(tmpdir(), 'areev-fake-llm-'))
      const script = join(dir, 'fake_llm.py')
      writeFileSync(script, FAKE_LLM_PY)
      return `${py} ${script}`
    }
  }
  return null
}

test('remember with prelinked facts carries no model attribution', async () => {
  const m = makeDb()
  const facts = JSON.stringify([
    { subject: 'john', relation: 'likes', object: 'tea', confidence: 0.9 },
  ])
  const res = JSON.parse(await m.remember('John likes tea', facts))
  assert.equal(res.facts.length, 1)
  assert.equal(res.event.length, HEX64)
  assert.equal(res.model, undefined, 'no model ran')
  // The raw text is an Event grain — same as the MCP tool and capture-stop.
  const rows = JSON.parse(await m.cal('RECALL events'))
  assert.equal(rows.grains[0].fields.content, 'John likes tea')
  // An incomplete triple is rejected rather than stored with empty fields.
  await assert.rejects(() => m.remember('note', JSON.stringify([{ subject: 'john' }])), Error)
})

test('remember threads a turn by session and role', async () => {
  const m = makeDb()
  // (content, factsJson, observer, ns, model, llmCmd, groundModel, groundCmd,
  //  extractHint, minConfidence, sessionId, role)
  await m.remember("what's the refund policy?", null, null, null, null, null, null, null, null, null, 'call-1', 'user')
  const rows = JSON.parse(await m.cal('RECALL events'))
  assert.equal(rows.grains[0].fields.session_id, 'call-1')
  assert.equal(rows.grains[0].fields.role, 'user')
})

test('remember extracts facts with a model', async (t) => {
  const llmCmd = fakeLlm()
  if (!llmCmd) return t.skip('no python on PATH')
  const m = makeDb()
  const res = JSON.parse(
    await m.remember('I always want a window seat.', null, null, null, null, llmCmd),
  )
  assert.equal(res.model, 'fake-extractor-1')
  assert.equal(res.proposed, 2)
  assert.equal(res.dropped, 0)
  assert.equal(res.verificationStatus ?? res.verification_status, 'unverified')
  assert.equal(res.facts.length, 2)

  const rows = JSON.parse(await m.cal('RECALL facts WHERE verification_status = "unverified"'))
  assert.equal(rows.grains.length, 2)
  for (const g of rows.grains) {
    assert.equal(g.fields.derived_from, res.event)
    assert.equal(g.fields.extractor_model, 'fake-extractor-1')
  }
})

test('remember grounding drops unsupported facts', async (t) => {
  const llmCmd = fakeLlm()
  if (!llmCmd) return t.skip('no python on PATH')
  const m = makeDb()
  // (content, factsJson, observer, ns, model, llmCmd, groundModel, groundCmd)
  const res = JSON.parse(
    await m.remember('I always want a window seat.', null, null, null, null, llmCmd, null, llmCmd),
  )
  assert.equal(res.proposed, 2)
  assert.equal(res.dropped, 1)
  assert.equal(res.facts.length, 1)
  assert.equal(res.verificationStatus ?? res.verification_status, 'verified')
})

test('memoryTool create/view over /memories', async () => {
  const m = makeDb()
  const created = await m.memoryTool(
    JSON.stringify({
      command: 'create',
      path: '/memories/prefs.md',
      file_text: 'Dark roast only.',
    }),
  )
  assert.match(created, /Created \/memories\/prefs\.md/)
  const listing = await m.memoryTool(JSON.stringify({ command: 'view', path: '/memories' }))
  assert.match(listing, /\/memories\/prefs\.md/)
  const body = await m.memoryTool(JSON.stringify({ command: 'view', path: '/memories/prefs.md' }))
  assert.match(body, /Dark roast only\./)
})

test('migrate mem0 export + history builds a supersession chain', async () => {
  const m = makeDb('main')
  const history = JSON.stringify([
    { memory_id: 'm-1', event: 'ADD', new_memory: 'Works at Acme', created_at: '2024-03-01T10:00:00Z' },
    { memory_id: 'm-1', event: 'UPDATE', new_memory: 'Works at Initech', created_at: '2024-06-01T10:00:00Z' },
  ])
  const rep = JSON.parse(await m.migrate('mem0-history', history, null, 'main'))
  assert.equal(rep.added, 1)
  assert.equal(rep.superseded, 1)

  const head = JSON.parse(await m.latest('mem0/m-1', 'mem0_memory', 'main'))
  assert.equal(head.fields.context.content, 'Works at Initech')
  const versions = JSON.parse(await m.history('mem0/m-1', 'mem0_memory', 'main'))
  assert.equal(versions.length, 2)

  // re-run is a no-op, not an error
  const rep2 = JSON.parse(await m.migrate('mem0-history', history, null, 'main'))
  assert.equal(rep2.added, 0)
})

test('latest resolves null when there is no head', async () => {
  const m = makeDb()
  assert.equal(await m.latest('nobody', 'prefers'), null)
})

test('reindexText succeeds on a text-indexed file', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  assert.equal(typeof (await m.reindexText()), 'number')
})

test('passphrase constructor rejects a wrong key on reopen', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'areev-js-enc-'))
  const path = join(dir, 'enc.db')
  {
    const m = new Areev(path, 'caller', 'correct horse battery staple')
    await m.addFact('john', 'prefers', 'tea')
    // One memory is one writer, and Node has no deterministic drop — leaving
    // scope does not release the file, so say so explicitly before reopening.
    m.close()
  }
  // wrong passphrase or no passphrase must not open the file. The constructor
  // stays synchronous, so these still throw rather than reject.
  assert.throws(() => new Areev(path, 'caller', 'wrong passphrase'), Error)
  assert.throws(() => new Areev(path, 'caller'), Error)
  // correct passphrase reads it back
  const m = new Areev(path, 'caller', 'correct horse battery staple')
  assert.equal(JSON.parse(await m.recall('john')).length, 1)
})

test('loop: record tool calls, run, review, apply', async () => {
  const m = makeDb('caller')
  // 4 failures + 1 success for one tool → tool-failure clustering fires.
  // The failures are byte-identical on purpose: an agent retrying a failing
  // call with the same arguments is the workload this analyzer exists for, and
  // `recordToolCall` stamps each one as its own occurrence so they no longer
  // collapse to a single grain (#66). This loop used to jitter its payloads to
  // work around that.
  for (let i = 0; i < 4; i++) {
    await m.recordToolCall('stripe_refund', 'rate_limited 429', true)
  }
  await m.recordToolCall('stripe_refund', 'ok', false)

  const run = JSON.parse(await m.loopRun())
  assert.equal(run.outcome, 'ran')
  assert.ok(run.stored >= 1)

  const pending = JSON.parse(await m.recommendations())
  const tf = pending.find((r) => r.analyzer.startsWith('loop.tool_failure'))
  assert.ok(tf, 'a tool-failure recommendation')
  assert.ok(tf.summary.includes('rate_limited'), 'signature is non-empty')

  const applied = JSON.parse(await m.applyRecommendation(tf.hash, 'retries belong in the client'))
  assert.equal(applied.hash, tf.hash)

  // A second bare run is idempotent (dedup).
  assert.equal(JSON.parse(await m.loopRun()).stored, 0)

  // The Verify gate's record parses (empty until checkpoints elapse) and an
  // applied recommendation rolls back with a mandatory reason.
  assert.ok(Array.isArray(JSON.parse(await m.loopOutcomes())))
  const rb = JSON.parse(await m.rollbackRecommendation(tf.hash, 'the lesson did not help'))
  assert.equal(rb.status, 'rolled_back')

  // A full-memory sweep (reflect semantics) still runs after the rollback.
  const sweep = await m.loopRun(null, null, null, null, null, null, null, null, true)
  assert.equal(JSON.parse(sweep).outcome, 'ran')
})

test('graph traversal, as-of reads, and workflow execution records', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'areev-graph-'))
  const m = new Areev(join(dir, 'm.db'), 'org')

  // A small org graph. `reports_to` is one of the default entity-valued
  // relations, so reverse traversal has an index to walk.
  await m.add('fact', JSON.stringify({ subject: 'alice', relation: 'reports_to', object: 'bob' }))
  await m.add('fact', JSON.stringify({ subject: 'bob', relation: 'reports_to', object: 'carol' }))

  const out = JSON.parse(await m.related('alice', 'reports_to', 'out', 2, 10))
  assert.deepEqual(out.reached, ['bob', 'carol'], 'two hops up the chain')

  const back = JSON.parse(await m.related('carol', 'reports_to', 'in', 2, 10))
  assert.deepEqual(back.reached, ['bob', 'alice'], 'reverse traversal via the OSP index')

  // Comma-separated relation lists, and a rejected direction.
  assert.ok(JSON.parse(await m.related('alice', 'reports_to, mg:knows')).reached.length >= 1)
  await assert.rejects(() => m.related('alice', 'reports_to', 'sideways'))
  await assert.rejects(() => m.related('alice', '  '))

  // As-of read. Nothing was recorded with explicit validity, so the world axis
  // finds nothing — but it must answer, not throw.
  const at = JSON.parse(await m.entityAt('alice', 'reports_to', Date.now(), 'knowledge'))
  assert.equal(at.found, true)
  assert.equal(at.grain.fields.object, 'bob')
  await assert.rejects(() => m.entityAt('alice', 'reports_to', Date.now(), 'sometime'))

  // Workflow execution records: the plan is immutable, so runs point at it.
  const wf = JSON.parse(
    await m.cal('ADD workflow "ci" build -> test REASON "smoke"'),
  )
  const wfHash = wf.hash
  assert.equal(wfHash.length, HEX64)

  const empty = JSON.parse(await m.stepActions(wfHash))
  assert.deepEqual(empty.steps, [], 'no runs recorded yet')
  assert.equal(empty.workflow, wfHash)

  await assert.rejects(() => m.stepActions('not-a-hash'))
})

test('the join: run history and semantic memory queried together', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'areev-join-'))
  const m = new Areev(join(dir, 'm.db'), 'ops')

  const ev = await m.add('event', JSON.stringify({
    content: 'caller asked about refunds',
    run_id: 'run-a',
  }))
  assert.equal(ev.length, HEX64)

  const fact = await m.add('fact', JSON.stringify({
    subject: 'refunds', relation: 'window_days', object: '30',
    derived_from: ev,
  }))

  // Forward: what the run recorded, and what it produced downstream.
  const trace = JSON.parse(await m.runTrace('run-a'))
  assert.equal(trace.run_id, 'run-a')
  assert.equal(trace.trace.length, 1)
  assert.equal(trace.trace[0].fields.run_id, 'run-a')
  assert.equal(trace.produced.length, 1, 'the distilled fact is not in the run')
  assert.equal(trace.produced[0].fields.object, '30')

  // The yield can be skipped when only the transcript is wanted.
  assert.deepEqual(JSON.parse(await m.runTrace('run-a', 64, false)).produced, [])

  // Reverse: from a fact back to the runs that produced it.
  const back = JSON.parse(await m.runsTouching(fact))
  assert.equal(back.hash, fact)
  assert.deepEqual(back.runs, ['run-a'])

  // Unknown runs answer, they do not throw.
  assert.deepEqual(JSON.parse(await m.runTrace('no-such-run')).trace, [])
  await assert.rejects(() => m.runsTouching('not-a-hash'))
})

test('reindexLinks rebuilds the join indexes and is idempotent', async () => {
  const m = makeDb()
  await m.remember('the caller asked about refunds', null, 'node', null, null, null, null, null, null, null, null, null, 'run-a')
  const rows = await m.reindexLinks()
  assert.equal(typeof rows, 'number')
  assert.equal(await m.reindexLinks(), rows)
})

test('remember can place a turn in a run that runTrace reads back', async () => {
  // run_trace/runYield/runsTouching shipped with no way to *write* a run id,
  // so from Node they could only ever answer "nothing".
  const m = makeDb()
  const args = [null, 'node', null, null, null, null, null, null, null, null, null]
  await m.remember('the caller asked about refunds', ...args, 'run-a')
  await m.remember('the agent quoted the 30-day policy', ...args, 'run-a')
  await m.remember('unrelated turn', ...args, 'run-b')

  const a = JSON.parse(await m.runTrace('run-a'))
  assert.equal(a.run_id, 'run-a')
  assert.equal(a.trace.length, 2)
  assert.equal(JSON.parse(await m.runTrace('run-b')).trace.length, 1)
  assert.deepEqual(JSON.parse(await m.runTrace('no-such-run')).trace, [])
})

test('one handle per file: a second open is refused, close() releases it', async () => {
  // Two handles on one file used to open silently and then collide on a write,
  // failing whichever handle wrote next rather than the one that should not
  // exist. Now it is decided at open.
  const dir = mkdtempSync(join(tmpdir(), 'areev-js-one-'))
  const path = join(dir, 'm.db')

  const a = new Areev(path, 'caller')
  await a.addFact('a1', 'rel', 'v')
  assert.throws(() => new Areev(path, 'caller'), /STO-E002/)

  // The refusal lands on the newcomer; the incumbent keeps working.
  await a.addFact('a2', 'rel', 'v')
  assert.equal(JSON.parse(await a.recall('a2')).length, 1)

  a.close()
  const b = new Areev(path, 'caller')
  assert.equal(JSON.parse(await b.recall('a1')).length, 1)

  // A closed handle reports that plainly rather than reopening behind your back.
  await assert.rejects(() => a.recall('a1'), /closed/)
  a.close() // idempotent
  b.close()
})

test('nearest without an embedder names the Node remedy, not a CLI flag', async () => {
  // The store's message used to name `--embed-cmd`, an `areev` CLI flag that
  // does not exist here — npm ships no binary — and called this a "novelty
  // check" when the caller had reached it through nearest().
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  await assert.rejects(
    () => m.nearest('tea', null, null, 3),
    (e) => {
      const msg = String(e.message ?? e)
      assert.match(msg, /nearest\(\)/)
      assert.match(msg, /setEmbedderCommand/)
      assert.ok(!msg.includes('--embed-cmd'), msg)
      assert.ok(!msg.includes('novelty check'), msg)
      return true
    },
  )
})

test('valid_to set at top level is stored as the typed field', async () => {
  // It used to be swallowed into context as the compacted key "vt", where
  // loop.staleness and the world-time projection cannot see it.
  const m = makeDb()
  const past = 1600000000000
  await m.add('fact', JSON.stringify({
    subject: 'promo', relation: 'code', object: 'SAVE20', valid_to: past,
  }))
  const fields = JSON.parse(await m.recall('promo'))[0].fields
  assert.equal(fields.valid_to, past)
  assert.ok(!JSON.stringify(fields.context ?? {}).includes('vt'), JSON.stringify(fields))
})

// ── Areev Loop: the review queue and its gates (#34, #35, #39) ──────────────────

async function aDestructiveRec() {
  // loop.staleness on an expired fact. `vt` is the compact spelling of
  // valid_to; both reach common.valid_to, and this one keeps the fixture
  // independent of the top-level routing fix (#36), which lands separately.
  const m = makeDb()
  await m.add('fact', JSON.stringify({
    subject: 'promo', relation: 'code', object: 'SAVE20', vt: 1600000000000,
  }))
  await m.loopRun()
  const recs = JSON.parse(await m.recommendations())
  assert.ok(recs.length && recs[0].destructive, JSON.stringify(recs))
  return { m, hash: recs[0].hash }
}

test('recommendations status "all" spans every state', async () => {
  // `"all"` used to filter itself to None and then have the pending default
  // re-applied, so it behaved as `"pending"`.
  const { m, hash } = await aDestructiveRec()
  await m.dismissRecommendation(hash, 'not now')

  const statuses = (f) => JSON.parse(f).map((r) => r.status)
  assert.deepEqual(statuses(await m.recommendations('{"status":"all"}')), ['rejected'])
  assert.deepEqual(statuses(await m.recommendations('{"status":"rejected"}')), ['rejected'])
  assert.deepEqual(statuses(await m.recommendations()), [])
  await assert.rejects(() => m.recommendations('{"status":"nonsense"}'), /unknown status/)
})

test('a refused destructive apply leaves the rec dismissable', async () => {
  // The fused approve+apply recorded the approval FIRST, then hit the
  // destructive gate — stranding the rec in `approved`, which has no exit but
  // `applied` or `expired`, so it could no longer be rejected.
  const { m, hash } = await aDestructiveRec()
  await assert.rejects(() => m.applyRecommendation(hash, 'looks right'), /LOP-E023/)

  const after = JSON.parse(await m.recommendations('{"status":"all"}')).map((r) => r.status)
  assert.deepEqual(after, ['pending'], 'a refused apply must not record the approval')
  assert.equal(JSON.parse(await m.dismissRecommendation(hash, 'legal hold')).status, 'rejected')
})

test('approve without apply, and scopes enforce separation of duties', async () => {
  const { m, hash } = await aDestructiveRec()
  await assert.rejects(
    () => m.applyRecommendation(hash, 'no apply scope', false, 'review'),
    /LOP-E022/,
  )
  await assert.rejects(() => m.approveRecommendation(hash, 'wrong scope', 'apply'), /LOP-E022/)
  await assert.rejects(() => m.approveRecommendation(hash, 'typo', 'reviewer'), /unknown scope/)

  const out = JSON.parse(await m.approveRecommendation(hash, 'evidence checks out', 'review'))
  assert.equal(out.status, 'approved')
  assert.deepEqual(
    JSON.parse(await m.recommendations('{"status":"all"}')).map((r) => r.status),
    ['approved'],
  )
})

test('loop health and per-analyzer config are reachable', async () => {
  const { m } = await aDestructiveRec()
  const health = JSON.parse(await m.loopHealth())
  assert.equal(health.pending, 1)
  assert.equal(health.applied, 0)
  assert.equal(health.stale, false)

  // The roster is how a caller learns the id, which carries a version suffix.
  const ids = JSON.parse(await m.loopAnalyzers()).map((a) => a.id)
  assert.ok(ids.includes('loop.staleness/1'), JSON.stringify(ids))

  await m.setAnalyzerConfig('loop.staleness/1', false)
  const off = Object.fromEntries(
    JSON.parse(await m.loopAnalyzers()).map((a) => [a.id, a.enabled]),
  )
  assert.equal(off['loop.staleness/1'], false)
  await m.setAnalyzerConfig('loop.staleness/1', true)
})

test('addBatch writes many grains in one transaction, JSON hashes out', async () => {
  const m = makeDb('batch')
  const grains = JSON.stringify([
    { grain_type: 'fact', fields: { subject: 'a', relation: 'r', object: 'one' } },
    { type: 'fact', fields: { subject: 'b', relation: 'r', object: 'two' } },
  ])
  const hashes = JSON.parse(await m.addBatch(grains))
  assert.equal(hashes.length, 2)
  for (const h of hashes) assert.equal(h.length, HEX64)
  const rows = JSON.parse(await m.recall('a', 'r'))
  assert.equal(rows.length, 1)
  // A malformed entry rejects before anything is written.
  await assert.rejects(() => m.addBatch(JSON.stringify([{ fields: {} }])), /grain_type/)
  m.close()
})

test('setEmbedder installs a JS callback and the vector leg answers', async () => {
  // indexText off: search would refuse with no legs, so a successful
  // search here proves the JS-callback vector leg is live — including the
  // worker→JS-thread round trip under the hood. (Ranking quality is the
  // real embedder's job; a 4-dim toy only proves plumbing, so assert
  // membership, not order.)
  const dir = mkdtempSync(join(tmpdir(), 'areev-js-'))
  const m = new Areev(join(dir, 'vec.db'), 'vec', null, null, null, null, false)
  m.setEmbedder((text) => {
    const v = [0, 0, 0, 0]
    for (let i = 0; i < text.length; i++) v[i % 4] += text.charCodeAt(i)
    return v
  }, 'byte-test')
  await m.addFact('john', 'prefers', 'window seat')
  await m.addFact('jane', 'prefers', 'strong coffee')
  const rows = JSON.parse(await m.search('prefers a window seat'))
  assert.ok(rows.length >= 1, 'the vector leg must answer')
  assert.ok(
    rows.some((r) => r.fields.object === 'window seat'),
    `the stored preference must be reachable: ${JSON.stringify(rows)}`,
  )
  m.close()
})

test('search errors loudly when the file has neither leg', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'areev-js-'))
  const m = new Areev(join(dir, 'noleg.db'), 'caller', null, null, null, null, false)
  await m.addFact('john', 'prefers', 'tea')
  await assert.rejects(() => m.search('tea'), /text or vector leg/)
  m.close()
})

test('indexText on the constructor is a deliberate re-stamp', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'areev-js-'))
  const path = join(dir, 'stamp.db')
  const off = new Areev(path, 'caller', null, null, null, null, false)
  await off.addFact('john', 'prefers', 'tea')
  off.close()
  const on = new Areev(path, 'caller', null, null, null, null, true)
  const warnings = JSON.parse(await on.openWarnings())
  assert.ok(
    warnings.some((w) => /text/i.test(w)),
    `the re-stamp must be reported: ${JSON.stringify(warnings)}`,
  )
  on.close()
})

test('subjectReport mirrors erasure and subjectBundle is portable', async () => {
  // The DSAR read (GDPR Art. 15/20): the erasure selector in show-me mode.
  const m = makeDb()
  await m.addFact('pat', 'prefers', 'tea')
  await m.addFact('pat#visit1', 'note', 'arrived late')
  await m.addFact('mary', 'prefers', 'juice')

  const report = JSON.parse(await m.subjectReport('pat'))
  assert.equal(report.grains.length, 2, 'exact + partition key, never mary')
  assert.ok(report.identity_names.includes('pat#visit1'))
  for (const g of report.grains) {
    assert.equal(g.hash.length, HEX64)
    assert.notEqual(g.fields.subject, 'mary')
  }

  const bundle = join(mkdtempSync(join(tmpdir(), 'areev-js-')), 'pat.mgb')
  const stats = JSON.parse(await m.subjectBundle(bundle, 'pat'))
  assert.equal(stats.ops, 2)

  // Erasure removes exactly what the report showed.
  const erased = JSON.parse(await m.forgetSubject('pat'))
  assert.equal(erased.grains_erased, report.grains.length)
  assert.deepEqual(JSON.parse(await m.subjectReport('pat')).grains, [])
  await m.close()
})

test('areev run runtime: start, respond as second principal, resume, verify, shadow', async () => {
  // Wave-5 parity: the JS binding drives the governed runtime end to end.
  const m = makeDb('ops')
  const greet = await m.add('tool', JSON.stringify({
    tool_name: 'greet', kind: 'definition',
    tool_description: 'greets', created_at: 500,
  }), 'ops')
  const approve = await m.add('tool', JSON.stringify({
    tool_name: 'approve', kind: 'definition',
    tool_description: 'human approves', executor_kind: 'client', created_at: 501,
  }), 'ops')
  const wf = await m.add('workflow', JSON.stringify({
    nodes: ['greet', 'approve'],
    edges: [{ src: 'greet', dst: 'approve' }],
    bindings: { greet, approve },
    created_at: 502,
  }), 'ops')

  const session = JSON.parse(await m.runStart(
    wf, 'js-1', '{"who":"world"}', `printf '{"greeting":"hello"}'`))
  assert.ok(session.parked, 'client-gated node parks the run')
  const ask = session.parked.asks[0].tool_call_id

  // Separation of duties: the starter cannot approve their own run.
  await assert.rejects(
    m.runRespond('js-1', ask, '{"ok":true}', 'user:local'),
    /responder/,
  )
  await m.runRespond('js-1', ask, '{"ok":true}', 'user:officer')
  const done = JSON.parse(await m.runResume('js-1'))
  assert.equal(done.finished, 'Completed')

  const report = JSON.parse(await m.runVerify('js-1'))
  assert.equal(report.verified, true)
  assert.ok(JSON.parse(await m.runList()).includes('js-1'))
  const shadow = JSON.parse(await m.runShadow(['js-1']))
  assert.equal(shadow.all_consistent, true)
  assert.equal(shadow.effect_dispatches, 0)

  // §5.4 time-travel fork: seed a new run from js-1's superstep-1 checkpoint.
  const seed = await m.runFork('js-1', 'js-1-fork', 1)
  assert.equal(seed.length, HEX64)
  assert.ok(JSON.parse(await m.runList()).includes('js-1-fork'))

  // Kill switch returns its receipt as JSON (py/js parity).
  assert.deepEqual(JSON.parse(await m.runCancel('js-1-fork', 'drill')),
    { canceled: 'js-1-fork' })

  // A negative budget is refused, never silently "unlimited".
  await assert.rejects(
    m.runStart(wf, 'js-neg', '{}', null, -5),
    /maxTokens/,
  )

  // Change feed: hlc rides as a STRING — HLC values exceed 2^53, so a
  // numeric hlc would silently lose bits in JSON.parse.
  const ops = JSON.parse(await m.changesSince(0, 10))
  assert.ok(ops.length > 0)
  assert.equal(typeof ops[0].hlc, 'string')
  assert.ok(BigInt(ops[0].hlc) >= 0n, 'string hlc round-trips through BigInt')
  await m.close()
})

test('anonymizeText round trip (proposal P0)', async () => {
  const m = makeDb()
  const out = JSON.parse(await m.anonymizeText('mail a@b.co, pin number is 1462'))
  assert.equal(out.text, 'mail [EMAIL_1], pin number is [PIN_1]')
  assert.deepEqual(out.mapping, { '[EMAIL_1]': 'a@b.co', '[PIN_1]': '1462' })
  assert.equal(out.mapping_id.length, 16)

  const back = JSON.parse(
    await m.rehydrateText('sent to [EMAIL_1] after pin [PIN_1]', JSON.stringify(out.mapping)),
  )
  assert.equal(back.text, 'sent to a@b.co after pin 1462')
  assert.equal(back.replaced, 2)
  assert.deepEqual(back.unmatched, [])
  await m.close()
})

test('scanText respects policy and fails closed on a bad one', async () => {
  const m = makeDb()
  const dets = JSON.parse(await m.scanText('host 10.0.0.1 down')).detections
  assert.equal(dets[0].category, 'ipv4')

  const allowed = JSON.parse(
    await m.scanText('host 10.0.0.1 down', JSON.stringify({ categories: { ipv4: 'allow' } })),
  )
  assert.deepEqual(allowed.detections, [])

  // an unreadable policy is a hard VAL error, not a silent no-policy (D3)
  await assert.rejects(() => m.scanText('x', JSON.stringify({ scope: 'memory' })), /VAL-E001/)
  await assert.rejects(() => m.rehydrateText('x', '{"bad": 1}'), /VAL-E001/)
  await m.close()
})

test('anon policy egress round trip (proposal P1)', async () => {
  const m = makeDb()
  await m.add('fact', JSON.stringify({
    subject: 'caller:john', relation: 'prefers', object: 'call me at +1 415 555 0142',
  }))

  await m.setAnonPolicy('caller', JSON.stringify({ mode: 'egress' }))
  const policies = JSON.parse(await m.anonPolicies())
  assert.equal(policies[0].ns, 'caller')
  assert.equal(policies[0].policy.mode, 'egress')

  const flat = await m.recall('caller:john')
  assert.ok(!flat.includes('caller:john') && !flat.includes('415 555'), flat)
  assert.ok(flat.includes('[PERSON_1]'), flat)

  // In-process custody: the mapping is available for rehydration.
  const mappings = JSON.parse(await m.anonMappings())
  assert.ok(mappings.some((row) => Object.values(row.mapping).includes('caller:john')))
  const back = JSON.parse(
    await m.rehydrateText('ok [PERSON_1]', JSON.stringify(mappings[0].mapping)),
  )
  assert.equal(back.text, 'ok caller:john')

  await m.clearAnonPolicy('caller')
  assert.deepEqual(JSON.parse(await m.anonPolicies()), [])
  assert.ok((await m.recall('caller:john')).includes('caller:john'))

  // The floor is a host cap, no declared policy needed.
  await m.setAnonymizeEgressFloor(true)
  assert.ok(!(await m.recall('caller:john')).includes('caller:john'))
  await m.close()
})
