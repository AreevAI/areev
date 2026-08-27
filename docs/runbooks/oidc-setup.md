# Runbook — native OIDC login for the console

**When you need this.** Not for "we want SSO" on its own — trusted-header SSO
behind an authenticating proxy (oauth2-proxy, Pomerium, Authelia, Cloudflare
Access, IAP) covers logins, needs no dependency, and stays the documented
default. Reach for native OIDC when **either**:

1. there is nowhere to run an authenticating proxy; **or**
2. people need to **approve HITL asks**, and you want an approver's identity
   to be stronger than a shared proxy secret.

(2) is the real reason it exists. With trusted-header SSO the identity is
vouched for by one fleet-wide secret, so whoever holds that secret can approve
as anyone — which is why proxy-asserted identities are refused at
`run.respond` by default (`--sso-approvals deny`). An OIDC identity is proven
by a signature the console verified against the issuer's published key set, so
it may approve without any opt-in.

If nobody approves through the console, use the proxy and stop here.

---

## 1. Build with the feature

```bash
cargo install areev --features oidc
# or, from a checkout:
cargo build --release -p areev --features oidc
```

Without it, `--oidc-issuer` refuses at startup and names the flag.

## 2. Register the console with your IdP

Create a **confidential** (server-side) client. You need three things back:
the issuer URL, a client ID, and a client secret.

The redirect URI must be **exactly** what you will pass to
`--oidc-redirect-uri` — RFC 9700 requires exact matching, and the console does
not normalize it for you. It is your console's public URL plus
`/auth/callback`:

```
https://console.example.com/auth/callback
```

| Provider | Issuer | `--oidc-principal-claim` |
|---|---|---|
| Google Workspace | `https://accounts.google.com` | `email` (default) |
| Microsoft Entra ID | `https://login.microsoftonline.com/<tenant-id>/v2.0` | **`preferred_username`** |
| Okta | `https://<org>.okta.com` | `email` (default) |
| Keycloak | `https://<host>/realms/<realm>` | `email` (default) |

There is no provider-specific code in Areev, deliberately — discovery (RFC
8414) makes these config. If a provider needs special handling it gets a flag,
never a module.

### Two things that will bite you on Entra ID

**Use the tenant-specific issuer, never `common` or `organizations`.** Those
multi-tenant endpoints publish a *template* as their issuer — literally
`https://login.microsoftonline.com/{tenantid}/v2.0`, placeholder braces and
all. Areev refuses a discovery document whose `issuer` does not equal the one
you configured, because trusting the document there would let a hostile
response redirect the entire flow while still passing the per-token `iss`
check. So a `common` issuer fails at startup, by design. Multi-tenant sign-in
would need the issuer to be validated per-token against the `tid` claim rather
than pinned once, which is deliberately not built: a console is a
single-organization surface.

**`email` is an optional claim on Entra.** It is emitted only when the user
has a mail attribute or the optional claim is configured, so the default
`--oidc-principal-claim email` will fail for many tenants with *"id_token
carries no `email` claim"*. Use `preferred_username` (present for
essentially every user), or `sub` if you want an identifier that survives a
username change — at the cost of audit records that read as opaque ids.
Whichever you pick is what you `GRANT` to.

## 3. Grant the people who will log in

Identity comes from the IdP; **rights come from the memory file**. An
authenticated stranger has nothing until the file grants it:

```bash
areev cal --db ops.db \
  'GRANT read,run.respond ON ops TO "pat@example.com" WITH because("refund approvals")'
```

The principal is whatever `--oidc-principal-claim` names (default `email`).
Use `sub` instead if your directory reassigns addresses — it is stable, at the
cost of an audit record that reads as an opaque id.

To keep IdP identities visibly distinct from local ones, add
`--oidc-principal-prefix "sso:"` and grant `"sso:pat@example.com"`.

## 4. Start the console

```bash
export AREEV_OIDC_SECRET='…'          # the client secret; never on the command line

areev ui --db ops.db \
  --addr 127.0.0.1:7437 \
  --oidc-issuer https://accounts.google.com \
  --oidc-client-id '<client-id>' \
  --oidc-client-secret-env AREEV_OIDC_SECRET \
  --oidc-redirect-uri https://console.example.com/auth/callback \
  --allow-origin https://console.example.com
```

Then terminate TLS in front of it (the documented default) — or add
`--tls-cert/--tls-key` with the `tls` feature if there is nowhere to put a
proxy.

Notes on the flags:

- **The secret is named, never passed.** `--oidc-client-secret-env` takes a
  *variable name*; the value never reaches `argv`, shell history, or any
  `--tool-cmd` subprocess (it is registered as withheld at startup, like
  `--passphrase-env`).
- **`--allow-origin` is still required** for a non-loopback deployment. The
  Origin check is CSRF protection and is not lifted by anything else.
- **Discovery runs at startup.** If the console cannot reach the IdP it fails
  to start, loudly — rather than starting and failing every login.

## 5. Verify

```bash
# 1. Login redirects to the IdP with PKCE.
curl -sI 'http://127.0.0.1:7437/auth/login' | grep -i location
#    → Location: https://accounts.google.com/o/oauth2/v2/auth?...code_challenge_method=S256...

# 2. In a browser: open the console, land on the IdP, come back signed in.

# 3. Confirm who you are and what you may do.
#    (from the browser's devtools console, so the session cookie is attached)
fetch('/api/whoami').then(r => r.json()).then(console.log)
#    → { principal: "pat@example.com", identity_source: "oidc", may_approve: true, ... }

# 4. Logout invalidates server-side, not just in the browser.
```

`identity_source` is the field to check. `oidc` means the signature was
verified here; `sso` means a proxy vouched; `sso-group` means a role, which
can never approve; `credential` is a per-principal token.

## 6. Operating it

- **Session lifetime** is 8h idle / 24h absolute, not configurable. Idle alone
  would let a stolen cookie live indefinitely as long as it kept being used.
- **Key rotation** needs nothing from you: an unknown `kid` triggers one JWKS
  refetch (rate-limited to once per 5 minutes, so unknown-kid tokens cannot be
  used to hammer your IdP).
- **Revoking someone** is a `REVOKE` in the memory file, or removing them at
  the IdP. Their existing session survives until it expires — Areev does not
  poll the IdP. If you need immediate cutoff, restart the console; sessions
  are in-process and do not survive it.
- **Restarting logs everyone out.** Sessions are in-memory by design (nothing
  auth-related is ever persisted in a memory file — invariant 5).

## 7. What this does not do

No SAML, no SCIM or directory sync, no user provisioning, no MFA, no password
storage, no refresh-token handling, no token issuance to third parties. Areev
is an OIDC **client**, never an authorization server. Machines keep bearer
tokens (`areev auth mint`) — there is no OIDC path for the CLI, MCP, or the
bindings, and the MCP spec itself says stdio servers should take credentials
from the environment rather than doing OAuth.

## Related

- [`../security-model.md`](../security-model.md) — the auth surface in full
- [`../auth-proposal.md`](../auth-proposal.md) — why this is scoped the way it is
- [`sso-secret-rotation.md`](sso-secret-rotation.md) — the trusted-header path
- [`../deployment-profile.md`](../deployment-profile.md) — where the console sits
