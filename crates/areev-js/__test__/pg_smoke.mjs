// Postgres-backend smoke for the napi binding: the SAME Areev class over a
// `postgres://…?schema=<name>` DSN. Needs a reachable server (pgvector image
// recommended):
//   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16
//   export DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres
// Skips without AREEV_PG_URL/DATABASE_URL.

import test from 'node:test'
import assert from 'node:assert/strict'

import { Areev, dropPostgresSchema } from '../index.js'

const url = process.env.AREEV_PG_URL || process.env.DATABASE_URL
const skip = !url || !url.startsWith('postgres')
const dsnFor = (schema) => `${url}${url?.includes('?') ? '&' : '?'}schema=${schema}`

test('postgres DSN drives the same API end-to-end', { skip }, async () => {
  const schema = `js_smoke_${process.pid}`
  try {
    const m = new Areev(dsnFor(schema), 'caller')
    const h = await m.addFact('luis', 'prefers', 'window seat')
    assert.equal(h.length, 64, 'content address comes back')
    const got = JSON.parse(await m.recall('luis'))
    assert.equal(got.length, 1)
    assert.equal(got[0].fields.object, 'window seat')
    const stats = JSON.parse(await m.stats())
    assert.equal(stats.grains, 1)
  } finally {
    dropPostgresSchema(url, schema)
  }
})

test('two instances write the same memory concurrently', { skip }, async () => {
  const schema = `js_multi_${process.pid}`
  try {
    const a = new Areev(dsnFor(schema), 'ns', undefined, undefined, 'off')
    const b = new Areev(dsnFor(schema), 'ns', undefined, undefined, 'off')
    await Promise.all([
      ...Array.from({ length: 10 }, (_, i) => a.addFact(`a${i}`, 'writes', 'ok')),
      ...Array.from({ length: 10 }, (_, i) => b.addFact(`b${i}`, 'writes', 'ok')),
    ])
    const stats = JSON.parse(await a.stats())
    assert.equal(stats.grains, 20, 'every concurrent write from both instances lands')
    // cross-instance visibility: b reads what a wrote
    assert.equal(JSON.parse(await b.recall('a3')).length, 1)
  } finally {
    dropPostgresSchema(url, schema)
  }
})

test('a passphrase with a DSN is a clear error', { skip }, () => {
  assert.throws(
    () => new Areev(dsnFor('never_created'), 'ns', 'secret'),
    /file-backed/,
    'page cipher is file-backend-only'
  )
})

test('subject erasure and retention sweep', { skip }, async () => {
  const schema = `js_erase_${process.pid}`
  try {
    const m = new Areev(dsnFor(schema), 'ns', undefined, undefined, 'off')
    await m.addFact('pat', 'condition', 'onset')
    await m.addFact('dr_lee', 'treats', 'pat')
    await m.addFact('mara', 'prefers', 'tea')
    const rep = JSON.parse(await m.forgetSubject('pat'))
    assert.equal(rep.grains_erased, 2, 'the chain and the referencing grain')
    assert.ok(rep.terms_removed >= 1, 'the identity dictionary entry')
    assert.deepEqual(JSON.parse(await m.recall('pat')), [])
    assert.deepEqual(JSON.parse(await m.recall('dr_lee')), [])
    assert.equal(JSON.parse(await m.recall('mara')).length, 1)
    const sweep = JSON.parse(await m.forgetOlderThan(Date.now() + 1000))
    assert.equal(sweep.grains_erased, 1)
    assert.equal(JSON.parse(await m.stats()).grains, 0)
  } finally {
    dropPostgresSchema(url, schema)
  }
})
