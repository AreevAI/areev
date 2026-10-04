// Stands in for areev-sandbox in the #374 test: a `wasm32-areev-io` module
// that calls an upstream through the broker under the `vendor` credential,
// once plainly and once trying to set that credential's own header itself.
// It reports both answers plus every value in its environment, so the test
// can assert the key never reached the module by any route.
import { readFileSync } from 'node:fs'

const at = process.argv.indexOf('--module')
const { upstream } = JSON.parse(readFileSync(process.argv[at + 1], 'utf8'))
const broker = process.env.AREEV_EGRESS_URL
const token = process.env.AREEV_EGRESS_TOKEN

async function ask(req) {
  if (!broker) return { status: 0, body: 'no broker' }
  const r = await fetch(broker, {
    method: 'POST',
    headers: { 'X-Areev-Egress-Token': token, 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
  })
  const text = await r.text()
  let body
  try { body = JSON.parse(text) } catch { body = text }
  return { status: r.status, body }
}

const out = {
  admitted: await ask({ url: upstream + '/ok', method: 'POST', credential: 'vendor', body: '{}' }),
  collision: await ask({
    url: upstream + '/ok', method: 'POST', credential: 'vendor',
    headers: { 'x-api-key': 'guest-chosen' }, body: '{}',
  }),
  env: Object.values(process.env).join('\n'),
}
process.stdout.write(JSON.stringify(out))
