// `packValidate` / `Areev#packInstall` (#341) — the Node mirror of
// crates/areev-py/tests/test_pack.py.
//
// The binding marshals; the install itself is `areev::pack::install_pack`, the
// function `areev pack install` prints. So what is pinned here is what a host
// depends on through THIS surface: a typed `err.code` on every refusal,
// all-or-nothing under the handle's bound principal, executor pins that are
// checked and never written, and — for every shipped example pack — the same
// plan hash the CLI prints for the same directory.

import test from 'node:test'
import assert from 'node:assert/strict'
import { tmpdir } from 'node:os'
import { dirname, join, relative } from 'node:path'
import { existsSync, mkdirSync, mkdtempSync, readdirSync, writeFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

import { Areev, packValidate } from '../index.js'

const REPO = fileURLToPath(new URL('../../..', import.meta.url))

function areevBin() {
  return process.env.AREEV_BIN
    || ['target/debug/areev', 'target/release/areev'].map((p) => join(REPO, p)).find(existsSync)
}

// Every `pack/` directory under examples/ that carries a pack.json.
function examplePacks() {
  const out = []
  const walk = (dir) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      if (!e.isDirectory() || e.name === 'node_modules') continue
      const p = join(dir, e.name)
      if (e.name === 'pack' && existsSync(join(p, 'pack.json'))) out.push(p)
      else walk(p)
    }
  }
  walk(join(REPO, 'examples'))
  return out.sort()
}

/** A code blob, a Definition naming it, a plan binding it, a saved query. */
function writePack(root, { expected } = {}) {
  mkdirSync(join(root, 'grains'), { recursive: true })
  mkdirSync(join(root, 'blobs'), { recursive: true })
  writeFileSync(join(root, 'blobs/poll.wasm'), Buffer.from('\0asm\x01\0\0\0not-a-real-module', 'binary'))
  writeFileSync(join(root, 'grains/010-tool.json'), JSON.stringify({
    type: 'tool', id: 'poll', kind: 'definition', tool_name: 'poll',
    tool_description: 'read the queue', created_at: 500,
    executor_uri: 'blob:poll', runtime: 'wasm32-areev-io',
    capabilities: [{ blob: { read: true } }],
  }))
  writeFileSync(join(root, 'grains/020-workflow.json'), JSON.stringify({
    type: 'workflow', id: 'plan', name: 'queue', nodes: ['poll'],
    bindings: { poll: 'grain:poll' }, created_at: 600,
  }))
  const wf = 'grains/020-workflow.json'
  writeFileSync(join(root, 'pack.json'), JSON.stringify({
    pack: 'queue', version: '1.0.0', namespace: 'ap',
    blobs: { poll: 'blobs/poll.wasm' },
    queries: { pulse: { body: 'RECALL facts LIMIT 5' } },
    grains: ['grains/010-tool.json', expected ? { file: wf, expected_hash: expected } : wf],
  }))
  return root
}

const plan = (r) => r.grains.filter((g) => g.grain_type === 'workflow').map((g) => g.hash).sort()
const tmp = () => mkdtempSync(join(tmpdir(), 'areev-pack-'))

async function toolCount(db) {
  return JSON.parse(await db.cal('RECALL tools WHERE namespace = "ap" RECENT 10')).total_available
}

test('packValidate opens no memory and reports the executors', async () => {
  const dir = tmp()
  const pack = writePack(join(dir, 'pack'))
  const r = JSON.parse(await packValidate(pack))
  assert.equal(r.pack, 'queue')
  assert.deepEqual(r.grains.map((g) => g.grain_type), ['tool', 'workflow'])
  assert.equal(r.executors.length, 1)
  assert.equal(r.executors[0].tool, 'poll')
  assert.equal(r.executors[0].pinned, false)
  assert.equal(r.executors[0].executor_uri, r.blobs[0].address)
  assert.deepEqual(r.registry, ['qry:pulse'])
  assert.deepEqual(readdirSync(dir), ['pack'], 'validate created a file')
})

test('a packValidate refusal carries the typed code', async () => {
  const pack = writePack(join(tmp(), 'pack'), { expected: '0'.repeat(64) })
  await assert.rejects(packValidate(pack), (e) => {
    assert.equal(e.code, 'PCK-E002')
    assert.match(e.message, /^PCK-E002/)
    return true
  })
})

test('packInstall matches validate; pins are checked, never written', async () => {
  const dir = tmp()
  const pack = writePack(join(dir, 'pack'))
  const validated = JSON.parse(await packValidate(pack))
  const addr = validated.executors[0].executor_uri

  const base = new Areev(join(dir, 'base.db'), 'ap')
  await base.packInstall(pack)
  const unpinned = await base.stats()
  base.close()

  const db = new Areev(join(dir, 'm.db'), 'ap')
  const r = JSON.parse(await db.packInstall(pack, {
    executorPins: { poll: addr }, expectedHash: plan(validated)[0],
  }))
  assert.deepEqual(plan(r), plan(validated), 'a pin or expectation moved the plan hash')
  assert.equal(r.executors[0].pinned, true)
  assert.equal(await db.stats(), unpinned, 'a pin added a write')
  db.close()
})

for (const [label, options, code] of [
  ['a mismatched pin', { executorPins: { poll: 'ab'.repeat(32) } }, 'PCK-E005'],
  ['a pin naming no code-carrying tool', { executorPins: { pol: 'ab'.repeat(32) } }, 'PCK-E005'],
  ['an unexpected plan', { expectedHash: '0'.repeat(64) }, 'PCK-E002'],
]) {
  test(`${label} refuses with ${code} and writes nothing`, async () => {
    const dir = tmp()
    const pack = writePack(join(dir, 'pack'))
    const addr = JSON.parse(await packValidate(pack)).blobs[0].address
    const db = new Areev(join(dir, 'm.db'), 'ap')
    const before = await db.stats()
    await assert.rejects(db.packInstall(pack, options), (e) => {
      assert.equal(e.code, code, e.message)
      return true
    })
    assert.equal(await db.stats(), before)
    assert.equal(await toolCount(db), 0)
    await assert.rejects(db.getBlob(addr))
    db.close()
  })
}

test('packInstall runs under the bound principal, all-or-nothing', async () => {
  const dir = tmp()
  const pack = writePack(join(dir, 'pack'))
  const addr = JSON.parse(await packValidate(pack)).blobs[0].address
  const path = join(dir, 'm.db')
  const owner = new Areev(path, 'ap')
  await owner.cal('GRANT read ON "ap" TO "user:reader"')
  await owner.cal('GRANT write ON "ap" TO "user:installer"')
  const before = await owner.stats() // memory-wide: the reader may not read it
  owner.close()

  const reader = new Areev(path, 'ap', undefined, undefined, undefined, 'user:reader')
  await assert.rejects(reader.packInstall(pack), (e) => {
    assert.equal(e.code, 'AUT-E001', e.message)
    return true
  })
  reader.close()

  const check = new Areev(path, 'ap')
  assert.equal(await check.stats(), before)
  assert.equal(await toolCount(check), 0)
  await assert.rejects(check.getBlob(addr), 'the blob landed ahead of the refusal')
  check.close()

  const installer = new Areev(path, 'ap', undefined, undefined, undefined, 'user:installer')
  const r = JSON.parse(await installer.packInstall(pack))
  assert.deepEqual(plan(r), plan(JSON.parse(await packValidate(pack))))
  installer.close()
})

test('dryRun checks everything and writes nothing', async () => {
  const dir = tmp()
  const pack = writePack(join(dir, 'pack'))
  const db = new Areev(join(dir, 'm.db'), 'ap')
  const before = await db.stats()
  const r = JSON.parse(await db.packInstall(pack, { dryRun: true }))
  assert.equal(r.grains.length, 2)
  assert.equal(await db.stats(), before)
  db.close()
})

test('every example pack installs at the plan hash the CLI prints', async (t) => {
  const bin = areevBin()
  if (!bin) {
    t.diagnostic('CLI leg skipped: no areev binary found (set AREEV_BIN)')
    return
  }
  const packs = examplePacks()
  assert.ok(packs.length >= 10, `expected the shipped example packs, found ${packs.length}`)
  for (const pack of packs) {
    const dir = tmp()
    const cli = spawnSync(bin, ['pack', 'install', pack, '--db', join(dir, 'cli.db'), '--format', 'json'],
      { encoding: 'utf8' })
    assert.equal(cli.status, 0, `${relative(REPO, pack)}: ${cli.stderr}`)
    const cliGrains = JSON.parse(cli.stdout).grains
    const cliPlans = cliGrains.filter((g) => g.type === 'workflow').map((g) => g.hash).sort()

    const validated = JSON.parse(await packValidate(pack))
    const executorPins = Object.fromEntries(validated.executors.map((x) => [x.tool, x.executor_uri]))
    const db = new Areev(join(dir, 'js.db'), 'shared')
    const installed = JSON.parse(await db.packInstall(pack, { executorPins }))
    db.close()
    assert.deepEqual(plan(installed), cliPlans, relative(REPO, pack))
    assert.deepEqual(plan(validated), cliPlans, relative(REPO, pack))
    assert.deepEqual(
      installed.grains.map((g) => g.hash).sort(),
      cliGrains.map((g) => g.hash).sort(),
      `${relative(REPO, pack)}: every grain, not only the plan`,
    )
    t.diagnostic(`${relative(REPO, dirname(pack))}: ${cliPlans.join(',') || '(no plan)'}`)
  }
})
