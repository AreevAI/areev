# Procurement table — the enterprise checklist, answered

The Wave-6 gate (governed-agents §8): every row a security/procurement
review asks about, answered green with the command that proves it, or
N/A with the rationale stated. Companion docs:
[`deployment-profile.md`](deployment-profile.md) (the reference
deployment), [`gdpr.md`](gdpr.md), [`eu-ai-act.md`](eu-ai-act.md),
[`security-model.md`](security-model.md).

| Requirement | Status | Evidence / rationale |
|---|---|---|
| Authentication on every service | ✅ | `areev ui --token-env` (Basic/Bearer on every request); `areev hub` makes the token mandatory; token-less consoles are read-only |
| Least privilege / RBAC | ✅ | Grants live IN the memory file as `mg:permits` Facts (CAL 1.3); per-principal credentials via `areev ui --auth areev-auth.json`; loop scopes derive from the bound principal's grants; per-memory token scoping (`memories` list) for shared auth files |
| Named actors on every change | ✅ | Every audit grain carries the resolved principal; shared-token approvals are structurally refused for `run.respond` and review — the approver's identity IS the record |
| Separation of duties | ✅ | Approval asks refuse responder == triggering principal (runtime-enforced); loop self-approval blocked; `write` grants neither review nor apply |
| Encrypted transport | ✅ | Documented default: TLS-terminating proxy (`deployment-profile.md`); native TLS via the non-default `tls` build feature (rustls; no plaintext downgrade — tested) |
| SSO | ✅ (v0) | Trusted-header auth behind an authenticating proxy (`--sso-header` + `--sso-secret-env`); forged headers without the proxy secret are ignored (tested). Native OIDC is a designed follow-on |
| SCIM / directory sync | N/A by design | Identity lives in the IdP; Areev maps identity → file grants. Provisioning = writing grant Facts (scriptable via CAL); tracked, not planned |
| Encryption at rest | ✅ | AES-256-GCM per file (`--passphrase-env`), Argon2id KDF; CAS blobs sealed under an HKDF subkey |
| Audit log, tamper-evident | ✅ | `areev audit export` — destruction trail + loop lifecycle as JSONL, hash-chain verified; truncation is flagged, never silent |
| Data retention with floors | ✅ | `areev retention` declarative policies (file-truths); `--min-days` floors + legal holds refuse destruction of the logs the policy cites |
| Right to erasure / DSAR | ✅ | `FORGET SUBJECT` and `REPORT SUBJECT` share ONE selector; erasure receipts; run-aware erasure truncates dependent checkpoints |
| Kill switch < 5 minutes | ✅ measured | `run.cancel` is the lowest-privilege verb; the drill test measures cancel→drain in seconds with work in flight; `areev run oversight-report` reports the journaled measurement per deployment |
| Human oversight (Art. 14) | ✅ | Client-gated nodes park runs for named approvers; `areev run oversight-report`; see `eu-ai-act.md` |
| Change control on agent code | ✅ | Rule E1: code changes pin their evalset, apply only with the recorded gating run, never auto-apply; `areev tool provenance` is one-command forensics |
| Re-acceptance after model swap | ✅ | `areev eval run --baseline RUN --tolerance N` — pass-rate comparison within tolerance bands, recorded as a grain, non-zero exit on failure |
| Reproducibility of executions | ✅ | `areev run verify` re-derives every checkpoint from the journal (labeled tiers; cross-arch caveat stated); `areev run shadow` replays with zero dispatches |
| Observability / OTel | ✅ | OTLP/HTTP JSON span export (`--otel-endpoint`) — one trace per run, resumes join the same trace; §6.10 event stream (`--events`) |
| Multi-tenant isolation | ✅ | One memory = one isolation unit (file, or Postgres schema with advisory-locked single writer); no cross-memory queries without explicit mounts |
| Supply-chain posture | ✅ | Dependency-light by policy (no HTTP framework/CLI framework/MCP SDK); `cargo deny` in CI; the two recorded exceptions (rustls; erasure) documented in ARCHITECTURE.md |
| Certifications (SOC 2, ISO 27001) | N/A — self-hosted | Areev is a library + self-hosted binaries; certifications attach to a hosted offering, which is a separate business decision (stated in the proposal, unchanged) |
| Penetration testing | Operator's scope | Self-hosted software: the deployment's pen test covers it; `SECURITY.md` has the vulnerability-report channel; the threat model is `security-model.md` |

Rows marked N/A are design positions, not gaps: each names the mechanism
that makes the requirement moot and where the responsibility actually
lives.
