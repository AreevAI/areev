# Security Policy

Areev stores memory for AI agents — often personal, long-lived, and sensitive
data. We take security seriously and appreciate responsible disclosure.

> **Status:** Areev is `1.0.0`. The security model below is honest
> about what is and is not protected today. Please read [Security model](#security-model)
> before deploying it anywhere beyond a local, trusted machine.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately through either channel:

1. **GitHub Security Advisories (preferred):**
   [Report a vulnerability](https://github.com/AreevAI/areev/security/advisories/new)
   — this opens a private advisory visible only to you and the maintainers.
2. **Email:** `security@areev.ai` with the subject line `Areev security`.

Please include, if you can: affected version/commit, a description of the
issue, reproduction steps or a proof of concept, and the impact you foresee.

### What to expect

| Stage | Target |
|---|---|
| Acknowledgement of your report | within **3 business days** |
| Initial assessment & severity triage | within **7 business days** |
| Fix or mitigation plan communicated | within **30 days** |

We will keep you informed throughout, credit you in the advisory and release
notes (unless you prefer to remain anonymous), and coordinate a disclosure
timeline with you. We support [coordinated disclosure](https://en.wikipedia.org/wiki/Coordinated_vulnerability_disclosure)
and ask for a reasonable window to ship a fix before public details are shared.

## Supported versions

The latest `1.x` release line receives security fixes.

| Version | Supported |
|---|---|
| `1.x` (latest) | ✅ |
| older | ❌ |

## Security model

Areev is an **embedded** engine. Its default and primary trust boundary is the
**local process and the user who runs it** — like SQLite, not like a networked
database server.

**In scope** (we want reports on these):

- Memory-safety or panics reachable from untrusted `.mg` blobs, bundles, or
  imported segments (the deserialization / sync path).
- Injection, path traversal, or auth bypass in the CAL executor, the store
  index layer, the MCP server, or the web console.
- Cryptographic weaknesses in the encryption-at-rest or crypto-erasure paths.
- Secret/data leakage in error messages, logs, or `Debug` output.
- Denial-of-service reachable from untrusted input (unbounded allocation,
  stack overflow, slowloris on the server).

**Out of scope / known limitations** (documented, not bugs):

- **The web console (`areev ui`) speaks plaintext HTTP.** Front it with a
  TLS-terminating reverse proxy for any non-loopback use. The plain `ui`
  console binds loopback with no auth by design; `--token-env` requires a
  bearer token on every request.
- **The `.blobs` CAS sidecar (large binary payloads) is stored in plaintext**
  even when the database itself is encrypted at rest. Encryption-at-rest
  currently protects the primary database file, not the blob sidecar.
- **Encryption-at-rest uses the underlying storage engine's AES-256-GCM**, which
  is an experimental feature of that dependency (Turso). Treat it as
  defense-in-depth, not a substitute for full-disk encryption.
- Grain **authenticity** is not verified on import — integrity relies on
  SHA-256 content addressing (detects corruption/tampering, not forged
  provenance). Only sync with peers you trust.
- Attacks requiring an already-compromised host, physical access, or a
  malicious local process with the same privileges as Areev.

A detailed threat model (data-at-rest, data-in-transit, multi-tenant, and the
sync trust model) lives in [`docs/security-model.md`](docs/security-model.md).

## Hardening checklist for operators

- Keep Areev and its dependencies up to date (`cargo update`; watch releases).
- Run `areev ui` on loopback only, or put a TLS proxy + auth in front.
- Set a strong bearer token on any exposed console (`areev ui --token-env`);
  rotate it periodically.
- Derive encryption keys from a strong secret and store them outside the
  database directory.
- Restrict filesystem permissions on the database directory and `.blobs`.
