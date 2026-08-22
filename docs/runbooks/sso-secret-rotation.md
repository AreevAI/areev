# Runbook: rotating the SSO proxy shared secret

**Applies to:** `areev ui --sso-header NAME --sso-secret-env VAR`
(trusted-header SSO v0).
**Audience:** whoever operates the authenticating proxy and the console.
**Time:** ~15 minutes planned; the compromise path is a single step you can
run in under a minute and clean up afterwards.

## What this secret is

The proxy does the OIDC/SAML handshake and forwards the resulting identity in
`--sso-header`. Areev trusts that header **only** when the same request carries
the shared secret in `x-areev-proxy-secret`. So the secret is not a password
for one account — it is the right to assert **any** identity, including
approval-capable principals whose signature is the audit record on a
human-in-the-loop decision.

Treat it exactly as you would a root credential: a secret manager, never in an
image or a repo, never shared across environments, and rotated on a schedule
rather than on an incident.

Two things it does **not** do, worth knowing before you plan a rotation:

- It grants no rights of its own. A proved identity still gets only what the
  **file** grants that principal (`mg:permits`). Rotating the secret changes
  who can *assert*, never who can *do*.
- It is per-instance host configuration. It is never written into the memory,
  never replicated in a bundle, and never visible to a CAL query — so a
  rotation touches your deployment, not your data.

## Before you start

- [ ] You can restart (or roll) every `areev ui` instance that has SSO enabled.
- [ ] You can change the secret at **every** proxy that fronts them — count
      them now, including any in a second region or a staging tier pointed at
      the same console.
- [ ] You have somewhere to generate a high-entropy value:
      `openssl rand -hex 32`.
- [ ] You know which environment variable each side reads. They do not have to
      match, and during a rotation they deliberately will not.

## A. Planned rotation (the two-secret window)

The console accepts **two** secrets while `--sso-secret-env-next` is set, so
the proxy fleet and the console never have to flip in the same instant. This is
TLS key rotation's shape: both valid briefly, the old one retired once nothing
presents it.

**1. Generate the new secret and store it.**

```bash
openssl rand -hex 32 | <your secret manager put> areev/sso-proxy-secret-next
```

**2. Open the window on every console instance.** The old secret stays exactly
where it is; the new one is added alongside.

```bash
export AREEV_SSO_SECRET=$(<secret manager get> areev/sso-proxy-secret)
export AREEV_SSO_SECRET_NEXT=$(<secret manager get> areev/sso-proxy-secret-next)

areev ui --db prod.db --ns caller \
  --sso-header X-Forwarded-User \
  --sso-secret-env AREEV_SSO_SECRET \
  --sso-secret-env-next AREEV_SSO_SECRET_NEXT
```

On start it says so, every time:

```
areev: trusted-header SSO enabled (X-Forwarded-User + x-areev-proxy-secret)
areev: ⚠ SSO secret ROTATION WINDOW open — two proxy secrets are accepted. …
```

**Verify before moving on.** Both must succeed, from a host the console will
accept:

```bash
for s in "$AREEV_SSO_SECRET" "$AREEV_SSO_SECRET_NEXT"; do
  curl -s -o /dev/null -w '%{http_code}\n' https://console.example.com/api/config \
    -H "X-Forwarded-User: user:you@example.com" \
    -H "x-areev-proxy-secret: $s"
done
# 200
# 200
```

If either is not `200`, stop: an instance did not pick up the change. Roll
forward until both answer `200` from every instance before touching a proxy.

**3. Move the proxies over, one at a time.** Change each proxy to send the new
secret, and confirm real traffic through that proxy still authenticates before
moving to the next. If one misbehaves, put the old value back on **that proxy**
— the window means it keeps working.

**4. Confirm nothing still presents the old secret.** This is the step that is
easy to skip and is the whole reason the window is dangerous to leave open.
Check the proxies' own configuration rather than inferring from console logs:
the console deliberately does not record which of the two matched (that would
be a timing and log oracle for a live credential).

```bash
# For every proxy in the fleet, not a sample:
<config management> get areev-proxy/* --field sso_secret | sort -u
# exactly one distinct value, and it is the NEW one
```

**5. Close the window.** Promote the new value to the primary variable and drop
the flag:

```bash
<secret manager> promote areev/sso-proxy-secret-next -> areev/sso-proxy-secret
export AREEV_SSO_SECRET=$(<secret manager get> areev/sso-proxy-secret)

areev ui --db prod.db --ns caller \
  --sso-header X-Forwarded-User \
  --sso-secret-env AREEV_SSO_SECRET
```

The startup warning disappears. That is the signal the rotation is finished —
if you still see it, you are still running with two live credentials.

**6. Verify the retired secret is dead.**

```bash
curl -s -o /dev/null -w '%{http_code}\n' https://console.example.com/api/config \
  -H "X-Forwarded-User: user:you@example.com" \
  -H "x-areev-proxy-secret: $OLD_SECRET"
# 401  (or a 200 that is ANONYMOUS — see the note below)
```

**7. Destroy the old value** in the secret manager, and record the rotation
wherever you record credential changes.

> **Reading the verification result.** A refused identity header is *ignored*,
> not rejected — the request proceeds as whatever its other credentials make
> it. So on a console with `--token-env` also configured you get `401`; on one
> without, you get `200` as an **anonymous** caller. Verify by attempting
> something only the granted principal may do (a write), not by the status code
> of a read.

## B. Suspected compromise (hard cutover)

If you believe the secret has leaked, **do not** open a window. A window means
the leaked value keeps working for its duration, which is exactly what you are
trying to end. Cut over hard and accept the brief failure.

**1. Decide the blast radius first — in one minute, not ten.** Anyone holding
that secret could have asserted any identity, so the exposure is *every* action
attributable to a proxied principal since the earliest time the leak could have
started. Note that timestamp now; you will need it for step 5.

**2. Rotate everywhere, at once.**

```bash
NEW=$(openssl rand -hex 32)
<secret manager put> areev/sso-proxy-secret "$NEW"
# restart every console instance AND every proxy with the new value
```

Between the first restart and the last, proxied requests fail closed: identity
headers stop being trusted and callers fall back to anonymous. Failing closed
is the correct behaviour here — a user seeing "not authorized" for ninety
seconds is a far better outcome than an attacker keeping approval-capable
identity for another hour.

If the console also has `--token-env`, rotate that token in the same pass. An
attacker who reached one host-side credential probably reached the others.

**3. Verify the old secret is dead**, as in A.6.

**4. Consider whether the *identities* need attention.** The secret proves the
proxy, not the person. If a specific principal's rights are the concern, the
answer is a grant change in the file, not another rotation:

```bash
areev cal 'REVOKE read,write ON caller FROM "user:pat@example.com" BECAUSE "…"' \
  --db prod.db --as user:admin
```

**5. Read the audit trail for the exposure window.** Every approval carries its
approver, and every destructive statement wrote a Tier-2 audit Observation, so
the record of what a forged identity did is in the memory itself:

```bash
areev audit export --db prod.db --since <the timestamp from step 1>
areev cal 'RECALL observations WHERE namespace = "agent:authz"' --db prod.db
areev run list --db prod.db          # HITL approvals: who approved what
```

**6. Write it up.** What leaked, how, the window, what the audit showed, and
what changed so it cannot recur.

## Why the window is two secrets and not more

Two is enough for old-and-new, and any more becomes a drawer of forgotten live
credentials — the failure this feature could otherwise cause rather than
prevent. The server also refuses a "rotation" whose new value equals the old
one: that is a rotation that did not happen, and it would read as *both live*
in every log and status line.

There is no expiry on the window. A deadline the server enforced would have to
live in the file (where host config deliberately never goes) or come from a
clock it does not own. Instead the console says the window is open on **every**
start, which is the loudest honest signal it can give without inventing state.

## What the code guarantees

| Claim | Where |
|---|---|
| Both configured secrets authenticate | `sso_tests::both_secrets_authenticate_during_a_rotation_window` |
| A retired secret stops working once the window closes | `sso_tests::the_retired_secret_stops_working_once_the_window_closes` |
| Rotating to the same value is refused | `sso_tests::rotating_to_the_same_value_is_refused` |
| A forged identity header without the secret is ignored | `sso_tests::proxied_identity_works_and_forged_headers_are_ignored` |
| The comparison is constant-time and does not short-circuit across the two | `UiServer::sso_secrets`, `ct_eq` |

Related: [deployment-profile.md](../deployment-profile.md#sso-note-trusted-header-mode),
[security-model.md](../security-model.md).
