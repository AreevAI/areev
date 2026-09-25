// A handle bound with `principal` fails closed on EVERY method
// (GHSA-rmrx-26f6-f97w) — the Node mirror of
// crates/areev-py/tests/test_principal_gating.py.
//
// `principal` binds the handle to the file's grants and is documented to fail
// closed (CAL 1.3 §9). Until 1.9.0 only CAL and five methods honoured it: the
// typed methods reached the store through `AreevFacade::with_store`, which
// applies no authorization, so a read-only principal could read any namespace,
// write through `remember()`, and ERASE through the memory tool's `delete`.
//
// The owner test is the positive control: gating that also refused the owner
// would pass the refusal test and break every existing caller.

import test from 'node:test'
import assert from 'node:assert/strict'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { mkdtempSync } from 'node:fs'

import { Areev } from '../index.js'

async function seed() {
  const dir = mkdtempSync(join(tmpdir(), 'areev-gate-'))
  const path = join(dir, 'gate.db')
  const owner = new Areev(path, 'open', undefined, undefined, undefined, undefined, true)
  await owner.cal(
    'ADD fact SET subject="deal:1" SET relation="stage" SET object="PUBLIC" ' +
      'SET namespace="open" BECAUSE "seed"',
  )
  await owner.cal(
    'ADD fact SET subject="deal:2" SET relation="stage" SET object="CLASSIFIED" ' +
      'SET namespace="secret" BECAUSE "seed"',
  )
  // amy exists as a principal but holds no grant at all.
  await owner.cal('GRANT read ON "unrelated" TO "user:amy"')
  owner.close()
  return { path, dir }
}

// Every gated method, aimed at the ungranted `secret` namespace.
function calls(db, dir) {
  const out = join(dir, 'out.mgb')
  const zero = '00'.repeat(32)
  return [
    // namespace-scoped reads
    ['recall', () => db.recall('deal:2', null, 5, 'secret')],
    ['latest', () => db.latest('deal:2', 'stage', 'secret')],
    ['search', () => db.search('CLASSIFIED', null, null, 5, 'secret')],
    ['history', () => db.history('deal:2', 'stage', 'secret')],
    ['related', () => db.related('deal:2', 'stage', 'out', 2, 10, 'secret')],
    ['entityAt', () => db.entityAt('deal:2', 'stage', 1, 'world', 'secret')],
    ['stepActions', () => db.stepActions(zero, null, 10, 'secret')],
    ['runTrace', () => db.runTrace('r1', 10, false, 'secret')],
    ['runGrains', () => db.runGrains('r1', 0, 10, 'secret')],
    ['runsTouching', () => db.runsTouching(zero, 2, 'secret')],
    ['threadTail', () => db.threadTail('s1', 5, 'secret')],
    ['nearest', () => db.nearest('CLASSIFIED', null, null, 5, 'secret')],
    ['nearestVector', () => db.nearestVector([0.1, 0.2], null, null, 5, 'secret')],
    // namespace-scoped writes and destruction
    ['remember', () => db.remember('planted', null, 'user:amy', 'secret')],
    ['migrate', () => db.migrate('mem0', '[]', null, 'secret')],
    ['telemetryScrubNamespace', () => db.telemetryScrubNamespace('secret')],
    ['setAnonPolicy', () => db.setAnonPolicy('secret', '{"mode":"off"}')],
    ['clearAnonPolicy', () => db.clearAnonPolicy('secret')],
    ['forgetSubject', () => db.forgetSubject('deal:2', 'secret', false)],
    ['subjectReport', () => db.subjectReport('deal:2', 'secret')],
    // the memory tool: one method, three verbs
    ['memoryTool view', () =>
      db.memoryTool(JSON.stringify({ command: 'view', path: '/memories' }), 'secret')],
    ['memoryTool create', () =>
      db.memoryTool(
        JSON.stringify({ command: 'create', path: '/memories/x.md', file_text: 'x' }),
        'secret',
      )],
    ['memoryTool delete', () =>
      db.memoryTool(JSON.stringify({ command: 'delete', path: '/memories/x.md' }), 'secret')],
    // memory-wide
    ['stats', () => db.stats()],
    ['verify', () => db.verify()],
    ['verifyAttestations', () => db.verifyAttestations()],
    ['putBlob', () => db.putBlob(Buffer.from('x'))],
    ['bundle', () => db.bundle(out, 0)],
    ['reindexText', () => db.reindexText()],
    ['reindexLinks', () => db.reindexLinks()],
    ['signingKey', () => db.signingKey()],
    ['setSigningKey', () => db.setSigningKey('11'.repeat(32))],
    ['setTrustedAuthors', () => db.setTrustedAuthors('{"keys":{},"policy":"off"}')],
    ['attestAll', () => db.attestAll(null)],
    ['addEmbeddings', () => db.addEmbeddings('[]')],
    ['dropVectorIndex', () => db.dropVectorIndex()],
    // LOWERING the floor is refused; raising it only strengthens protection
    // and needs no grant (#345).
    ['setAnonymizeEgressFloor', () => db.setAnonymizeEgressFloor(false)],
    // A decision backend can be a subprocess and receives memory text as its
    // state — host config, admin on "*" like the embedder.
    ['setDecider', () => db.setDecider(null, 'cat')],
    ['setRerankerCommand', () => db.setRerankerCommand('cat')],
  ]
}

async function settle(fn) {
  try {
    return { ok: true, value: await fn() }
  } catch (e) {
    return { ok: false, error: String(e?.message ?? e) }
  }
}

test('a principal with no grants is refused by every typed method', async () => {
  const { path, dir } = await seed()
  const amy = new Areev(path, 'open', undefined, undefined, undefined, 'user:amy')
  const leaked = []
  for (const [name, fn] of calls(amy, dir)) {
    const r = await settle(fn)
    if (r.ok) {
      leaked.push(`${name}: RETURNED ${String(r.value).slice(0, 120)}`)
    } else if (!r.error.includes('AUT-E001')) {
      // A different error means the call never reached the gate — a bad
      // signature here would hide a real hole.
      leaked.push(`${name}: non-authz error ${r.error.slice(0, 120)}`)
    }
  }
  amy.close()
  assert.deepEqual(leaked, [], 'unrefused calls under a zero-grant principal')
})

test('no classified content escapes to a restricted principal', async () => {
  const { path, dir } = await seed()
  const amy = new Areev(path, 'open', undefined, undefined, undefined, 'user:amy')
  for (const [name, fn] of calls(amy, dir)) {
    const r = await settle(fn)
    if (r.ok) {
      assert.ok(!String(r.value).includes('CLASSIFIED'), `${name} disclosed a secret grain`)
    }
  }
  amy.close()
})

test('owner session is unaffected by the gating (positive control)', async () => {
  const { path, dir } = await seed()
  const owner = new Areev(path, 'open', undefined, undefined, undefined, undefined, true)
  const refused = []
  for (const [name, fn] of calls(owner, dir)) {
    const r = await settle(fn)
    if (!r.ok && r.error.includes('AUT-E001')) refused.push(`${name}: ${r.error}`)
  }
  owner.close()
  assert.deepEqual(refused, [], 'owner session wrongly refused')
})

test('fail-closed is not fail-always: a grant is honoured', async () => {
  const { path } = await seed()
  const owner = new Areev(path, 'open', undefined, undefined, undefined, undefined, true)
  await owner.cal('GRANT read ON "open" TO "user:bob"')
  owner.close()

  const bob = new Areev(path, 'open', undefined, undefined, undefined, 'user:bob')
  const got = JSON.parse(await bob.recall('deal:1', null, 5, 'open'))
  assert.equal(got[0].fields.object, 'PUBLIC')
  await assert.rejects(() => bob.recall('deal:2', null, 5, 'secret'), /AUT-E001/)
  bob.close()
})
