# Security Policy

Areev stores memory for AI agents — often personal, long-lived, and sensitive
data. We take security seriously and appreciate responsible disclosure.

> **Status:** this policy covers the latest `1.x` release (see
> [`CHANGELOG.md`](CHANGELOG.md) for the current version). The security model below is honest
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

Security fixes ship in the latest `1.x` release. The previous minor gets a
limited, time-boxed window so operators have room to upgrade.

| Version | Supported |
|---|---|
| Latest minor (`1.N.x`) | ✅ All security fixes, released as a new patch. |
| Previous minor (`1.(N-1).x`) | ⚠️ Fixes for **critical and high** severity issues only, for **90 days** after the next minor ships, on a best-effort basis. |
| Older minors | ❌ Upgrade to the latest `1.x` release. |

Within `1.x`, the `.mg` format, CAL syntax and error codes are stable
contracts, so upgrading to the latest minor is the supported fix path. A
minor that migrates the Postgres schema cannot be rolled back in place; take
a backup before upgrading
([`docs/deployment-profile.md`](docs/deployment-profile.md#rolling-back-a-minor-release-after-a-schema-migration)).

## Known advisories

[`deny.toml`](deny.toml) fails the build on any RUSTSEC advisory in the
dependency graph. Each ignore must have a written justification and a
tracking issue. Two advisories are ignored today. Both reach the tree only
through `jsonwebtoken` under `areev-server`'s **non-default `oidc`**
feature. A default build does not contain the affected crates.

The reasoning is the same for both. Each advisory is a timing leak over
**secret** key material, and Areev's `jsonwebtoken` use is verification
only. The console is an OIDC *client*: it checks an id_token's signature
against the issuer's published **public** key. Every value in that
computation (public key, signature, message) is already public. The tree
contains no `EncodingKey`, no `encode` call and no private key material.
Areev never issues or signs tokens, which is an explicit non-goal
([`docs/auth-proposal.md`](docs/auth-proposal.md) §8).

| Advisory | Affected crate and path | Feature | Why it is not reachable | Remove the ignore when | Tracking |
|---|---|---|---|---|---|
| [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071) (Marvin attack) | `rsa`, via `jsonwebtoken`'s `rust_crypto` backend | `oidc` (non-default) | A **private-key** recovery attack. Areev holds no RSA private key and only verifies RS256 id_tokens with the issuer's public key. | `rsa` ships a constant-time release; or `jsonwebtoken` moves to a backend without `rsa` (for example `aws_lc_rs`, if the C-free/ARM build constraint stops mattering); or anything in the tree starts doing RSA private-key operations (signing, decryption), which would make the ignore invalid. | [#140](https://github.com/AreevAI/areev/issues/140) |
| [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387) | `crypto-bigint` 0.5.5 (fixed in ≥ 0.7.6), via `jsonwebtoken` 11.1.0 → `p256`/`p384` → `elliptic-curve` 0.13 | `oidc` (non-default) | Non-constant-time arithmetic over **secret** integers. The ES256/ES384 path only verifies, with public inputs, and Areev holds no EC private key. No semver-compatible update reaches `crypto-bigint` 0.7. | `jsonwebtoken` releases on `elliptic-curve` ≥ 0.14 / `crypto-bigint` ≥ 0.7.6; or anything in the tree starts doing EC private-key operations. | [#427](https://github.com/AreevAI/areev/issues/427) |

The ignores are workspace-wide, so they would also hide a real finding if a
future dependency reached either crate through a signing path. Both are
re-checked on every `jsonwebtoken` bump. #427 stays open until its removal
condition is met. #140 is closed, but its ignore and its conditions still
apply.

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
