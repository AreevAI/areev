//! Native OIDC for the console (A3) — behind the non-default `oidc` feature.
//!
//! **Why this exists at all.** The documented default is still an
//! authenticating proxy (SSO v0): it needs no dependency and covers most
//! deployments. This module exists for the one thing a proxy structurally
//! cannot do — carry an approver's identity. A forwarded identity header is
//! trusted via one shared, fleet-wide secret, so whoever holds that secret can
//! approve as anyone, and the resulting HITL audit record is indistinguishable
//! from a real one. An `id_token` is different in kind: the IdP signed it, and
//! the signature is checked against the issuer's published key set. That, and
//! only that, is what justifies the dependency.
//!
//! **Scope, hard-bounded** (`docs/auth-proposal.md` §6.1):
//! - authorization code + PKCE (S256) — per RFC 9700, on a confidential
//!   client too; exact redirect-URI matching; no implicit, no password grant.
//! - BFF-shaped: this process is the confidential client, tokens never leave
//!   it, and the browser gets an `HttpOnly` `SameSite=Strict` session cookie.
//!   Areev arrives here almost for free — the console is one server-rendered
//!   file with no SPA token store to leak.
//! - Areev is never an authorization server: no user database, no password
//!   storage, no MFA, no token issuance to third parties.
//! - Machines keep bearer tokens. This is for humans in the console.

use std::collections::HashMap;
use std::sync::Mutex;

use areev_core::error::{AreevError, Result};

/// Clock skew tolerated on `exp`/`nbf`/`iat`. Small on purpose: an IdP and a
/// console that disagree by more than a minute have an operational problem,
/// and a generous leeway is indistinguishable from a longer token lifetime.
const CLOCK_SKEW_SECS: u64 = 60;
/// How long a login may sit half-finished between `/auth/login` and
/// `/auth/callback`.
const PENDING_TTL_SECS: u64 = 10 * 60;
/// Bounded, like every other attacker-influenced map on this server: an
/// unauthenticated caller can start logins, so `pending` is key space they
/// control.
const MAX_PENDING: usize = 1024;
/// Bounded for the same reason, one step later in the flow.
const MAX_SESSIONS: usize = 4096;
/// Idle timeout — a session untouched this long is gone.
const SESSION_IDLE_SECS: u64 = 8 * 60 * 60;
/// Absolute lifetime — a session lives no longer than this even if used
/// constantly. Idle timeout alone lets a stolen cookie live forever as long
/// as the thief keeps using it.
const SESSION_ABSOLUTE_SECS: u64 = 24 * 60 * 60;
/// The session cookie's name.
pub const SESSION_COOKIE: &str = "areev_session";

/// Operator-supplied OIDC settings. Host config — never persisted in a
/// memory file (invariant 5).
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The issuer URL, e.g. `https://accounts.google.com`. Discovery hangs
    /// off it, and it is what every `id_token`'s `iss` must equal.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Exact redirect URI, registered verbatim with the IdP (RFC 9700:
    /// exact matching, no wildcards, no prefix rules).
    pub redirect_uri: String,
    /// Space-separated scopes. `openid` is mandatory and added if missing.
    pub scopes: String,
    /// Which claim becomes the principal. `email` by default because that is
    /// what a human recognizes in an audit record; `sub` is the stable
    /// choice when addresses get reassigned.
    pub principal_claim: String,
    /// Optional prefix on the resulting principal, so IdP identities stay
    /// visibly distinct from credential-map ones (mirrors A2's
    /// `--sso-principal-prefix`).
    pub principal_prefix: Option<String>,
}

/// The subset of RFC 8414 provider metadata this client uses. Discovery is
/// what makes Google and Entra *config* rather than two code paths.
#[derive(Debug, Clone, serde::Deserialize)]
struct ProviderMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// A login in flight. Everything here is per-attempt and short-lived.
struct Pending {
    /// PKCE verifier — the proof that the client redeeming the code is the
    /// one that started the flow.
    verifier: String,
    /// Replay defence, echoed in the `id_token` and compared.
    nonce: String,
    created: std::time::Instant,
}

/// An authenticated browser session.
pub struct Session {
    pub principal: String,
    created: std::time::Instant,
    last_seen: std::time::Instant,
}

/// The console's OIDC client.
pub struct OidcProvider {
    cfg: OidcConfig,
    meta: ProviderMetadata,
    /// Cached JWKS, refetched when a token names an unknown `kid` (key
    /// rotation) — rate-limited so an attacker cannot turn unknown-kid
    /// tokens into a fetch amplifier against the IdP.
    jwks: Mutex<(jsonwebtoken::jwk::JwkSet, std::time::Instant)>,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl OidcProvider {
    /// Discover the provider and build the client. Called once at startup —
    /// a console that cannot reach its IdP should fail to start, loudly,
    /// rather than fail every login later.
    pub fn discover(mut cfg: OidcConfig) -> Result<OidcProvider> {
        if !cfg.scopes.split_whitespace().any(|s| s == "openid") {
            cfg.scopes = format!("openid {}", cfg.scopes).trim().to_string();
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            cfg.issuer.trim_end_matches('/')
        );
        let body = http_get(&url)?;
        let meta: ProviderMetadata = serde_json::from_str(&body).map_err(|e| {
            AreevError::Validation(format!("OIDC discovery at {url}: malformed metadata: {e}"))
        })?;
        // The issuer in the document must match the one configured, or a
        // hostile discovery response could point every subsequent step at
        // an attacker's endpoints while still passing the `iss` check.
        if meta.issuer.trim_end_matches('/') != cfg.issuer.trim_end_matches('/') {
            return Err(AreevError::Validation(format!(
                "OIDC discovery at {url}: document declares issuer {:?} but {:?} was configured \
                 — refusing, because trusting the document here would let it redirect the whole \
                 flow",
                meta.issuer, cfg.issuer
            )));
        }
        // Every endpoint must be HTTPS. A plaintext token endpoint would put
        // the client secret and the code on the wire in the clear.
        for (name, url) in [
            ("authorization_endpoint", &meta.authorization_endpoint),
            ("token_endpoint", &meta.token_endpoint),
            ("jwks_uri", &meta.jwks_uri),
        ] {
            if !url.starts_with("https://") {
                return Err(AreevError::Validation(format!(
                    "OIDC discovery: {name} {url:?} is not https"
                )));
            }
        }
        let jwks = fetch_jwks(&meta.jwks_uri)?;
        Ok(OidcProvider {
            cfg,
            meta,
            jwks: Mutex::new((jwks, std::time::Instant::now())),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Whether the redirect URI is https — which is what decides the
    /// `Secure` cookie attribute. A loopback console over plain http must
    /// still work, so this is derived rather than assumed.
    pub fn is_secure(&self) -> bool {
        self.cfg.redirect_uri.starts_with("https://")
    }

    /// Begin a login: returns the IdP URL to redirect the browser to.
    ///
    /// `state` is the CSRF binding between this redirect and the callback
    /// that comes back; the PKCE verifier and the nonce are held server-side
    /// against it, so nothing security-relevant rides in the browser.
    pub fn authorize_url(&self) -> Result<String> {
        let state = random_token();
        let nonce = random_token();
        let verifier = random_token();
        let challenge = pkce_challenge(&verifier);

        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            prune_pending(&mut pending);
            if pending.len() >= MAX_PENDING {
                return Err(AreevError::Validation(
                    "too many logins in flight — try again in a moment".into(),
                ));
            }
            pending.insert(
                state.clone(),
                Pending { verifier, nonce: nonce.clone(), created: std::time::Instant::now() },
            );
        }

        Ok(format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}\
             &code_challenge={}&code_challenge_method=S256",
            self.meta.authorization_endpoint,
            urlencode(&self.cfg.client_id),
            urlencode(&self.cfg.redirect_uri),
            urlencode(&self.cfg.scopes),
            urlencode(&state),
            urlencode(&nonce),
            urlencode(&challenge),
        ))
    }

    /// Finish a login: exchange the code, validate the `id_token`, and mint
    /// a session. Returns the raw session id to put in the cookie.
    pub fn complete_login(&self, code: &str, state: &str) -> Result<String> {
        // Take the pending entry — one state, one redemption. Leaving it in
        // place would make the callback replayable.
        let pending = {
            let mut map = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            prune_pending(&mut map);
            map.remove(state).ok_or_else(|| {
                AreevError::Validation(
                    "unknown or expired login state — start again from /auth/login".into(),
                )
            })?
        };

        let id_token = self.exchange_code(code, &pending.verifier)?;
        let claims = self.validate_id_token(&id_token, &pending.nonce)?;

        let raw = claims
            .get(&self.cfg.principal_claim)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AreevError::Validation(format!(
                    "id_token carries no {:?} claim — set --oidc-principal-claim to one the \
                     provider actually issues (commonly `sub` or `email`)",
                    self.cfg.principal_claim
                ))
            })?;
        // The claim is IdP-controlled text on its way to an immutable audit
        // grain: the same sanitation the A2 header path applies.
        let principal = crate::sanitize_sso_identity(raw).ok_or_else(|| {
            AreevError::Validation(
                "id_token principal claim is empty, over-long, or a reserved name".into(),
            )
        })?;
        let principal = match &self.cfg.principal_prefix {
            Some(p) if !principal.starts_with(p.as_str()) => format!("{p}{principal}"),
            _ => principal,
        };

        let sid = random_token();
        let now = std::time::Instant::now();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prune_sessions(&mut sessions);
        if sessions.len() >= MAX_SESSIONS {
            return Err(AreevError::Validation("too many active sessions".into()));
        }
        // Stored under the DIGEST of the session id, not the id itself: a
        // process memory dump then yields no usable cookie. Same reasoning
        // as the credential map storing SHA-256 rather than tokens.
        sessions.insert(digest(&sid), Session { principal, created: now, last_seen: now });
        Ok(sid)
    }

    /// The principal for a presented session cookie, refreshing its idle
    /// clock. `None` for unknown, idle-expired, or age-expired sessions.
    pub fn principal_for_session(&self, sid: &str) -> Option<String> {
        if sid.is_empty() {
            return None;
        }
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prune_sessions(&mut sessions);
        let s = sessions.get_mut(&digest(sid))?;
        s.last_seen = std::time::Instant::now();
        Some(s.principal.clone())
    }

    /// Build a provider with a known-good session and no network, for tests
    /// of everything AROUND the IdP handshake — routing, cookies, session
    /// lifetime, and the approval rules an OIDC principal is subject to.
    ///
    /// Deliberately does not fake a signature: `id_token` verification is
    /// delegated to a vetted library precisely so it is not this crate's to
    /// re-implement or to mock. The parts that ARE this crate's — algorithm
    /// selection, nonce, PKCE, session handling — are tested directly.
    #[cfg(test)]
    pub(crate) fn with_test_session(cfg: OidcConfig, sid: &str, principal: &str) -> OidcProvider {
        let now = std::time::Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            digest(sid),
            Session { principal: principal.to_string(), created: now, last_seen: now },
        );
        OidcProvider {
            meta: ProviderMetadata {
                issuer: cfg.issuer.clone(),
                authorization_endpoint: format!("{}/authorize", cfg.issuer),
                token_endpoint: format!("{}/token", cfg.issuer),
                jwks_uri: format!("{}/jwks", cfg.issuer),
            },
            cfg,
            jwks: Mutex::new((
                jsonwebtoken::jwk::JwkSet { keys: Vec::new() },
                std::time::Instant::now(),
            )),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(sessions),
        }
    }

    /// Invalidate one session. This is what makes logout mean something: the
    /// cookie is cleared in the browser AND the server forgets the session,
    /// so a copy of the cookie taken beforehand is dead too.
    pub fn logout(&self, sid: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.remove(&digest(sid));
    }

    /// POST the code to the token endpoint and return the raw `id_token`.
    fn exchange_code(&self, code: &str, verifier: &str) -> Result<String> {
        let form = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}\
             &client_secret={}&code_verifier={}",
            urlencode(code),
            urlencode(&self.cfg.redirect_uri),
            urlencode(&self.cfg.client_id),
            urlencode(&self.cfg.client_secret),
            urlencode(verifier),
        );
        let body = http_post_form(&self.meta.token_endpoint, &form)?;
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| AreevError::Validation(format!("token endpoint: malformed JSON: {e}")))?;
        // The error body is the IdP's, and it names client_id and error
        // codes — useful — but never echo the whole body, which on some
        // providers repeats request parameters back.
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Err(AreevError::Validation(format!(
                "token endpoint refused the code: {err}"
            )));
        }
        v.get("id_token")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                AreevError::Validation(
                    "token endpoint returned no id_token — is `openid` in the scopes?".into(),
                )
            })
    }

    /// Validate signature, issuer, audience, lifetime and nonce.
    ///
    /// This is the entire security surface of the feature, which is why it
    /// runs through a vetted library rather than hand-rolled parsing.
    fn validate_id_token(
        &self,
        token: &str,
        expected_nonce: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| AreevError::Validation(format!("id_token header: {e}")))?;
        let kid = header.kid.ok_or_else(|| {
            AreevError::Validation("id_token has no `kid` — cannot select a signing key".into())
        })?;

        // ALGORITHM CONFUSION: the token header's `alg` is attacker-supplied,
        // so it may select a key but must never widen what is acceptable.
        // The classic attack is presenting `alg: HS256` and signing with the
        // issuer's *public* key as the HMAC secret — which succeeds anywhere
        // the verifier trusts the header. Symmetric algorithms have no place
        // in an OIDC id_token flow at all, so they are refused outright and
        // the token is then verified only under an asymmetric algorithm.
        if !is_asymmetric(header.alg) {
            return Err(AreevError::Validation(format!(
                "id_token declares symmetric algorithm {:?} — refused: an id_token from a \
                 JWKS-published key set must be asymmetrically signed",
                header.alg
            )));
        }

        let key = self.decoding_key(&kid)?;
        let mut validation = jsonwebtoken::Validation::new(header.alg);
        validation.set_issuer(&[self.meta.issuer.as_str()]);
        validation.set_audience(&[self.cfg.client_id.as_str()]);
        validation.leeway = CLOCK_SKEW_SECS;
        // `exp` is mandatory: a token with no expiry is a permanent one.
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);

        let data = jsonwebtoken::decode::<serde_json::Map<String, serde_json::Value>>(
            token,
            &key,
            &validation,
        )
        .map_err(|e| AreevError::Validation(format!("id_token rejected: {e}")))?;

        // Nonce binds this token to THIS login attempt. Without it a token
        // captured from another flow for the same client would be accepted.
        let nonce = data.claims.get("nonce").and_then(|n| n.as_str()).unwrap_or("");
        if !ct_eq_str(nonce, expected_nonce) {
            return Err(AreevError::Validation(
                "id_token nonce does not match this login attempt (replay refused)".into(),
            ));
        }
        Ok(data.claims)
    }

    /// The decoding key for `kid`, refetching the key set once if it is
    /// unknown (the IdP rotated) — but no more often than every 5 minutes,
    /// so an attacker replaying unknown-kid tokens cannot use this console
    /// to hammer the IdP.
    fn decoding_key(&self, kid: &str) -> Result<jsonwebtoken::DecodingKey> {
        {
            let guard = self
                .jwks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(jwk) = guard.0.find(kid) {
                return jsonwebtoken::DecodingKey::from_jwk(jwk)
                    .map_err(|e| AreevError::Validation(format!("JWKS key {kid}: {e}")));
            }
        }
        let mut guard = self
            .jwks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.1.elapsed() >= std::time::Duration::from_secs(300) {
            if let Ok(fresh) = fetch_jwks(&self.meta.jwks_uri) {
                guard.0 = fresh;
                guard.1 = std::time::Instant::now();
            }
        }
        let jwk = guard.0.find(kid).ok_or_else(|| {
            AreevError::Validation(format!(
                "id_token signed by unknown key {kid} — not in the issuer's published key set"
            ))
        })?;
        jsonwebtoken::DecodingKey::from_jwk(jwk)
            .map_err(|e| AreevError::Validation(format!("JWKS key {kid}: {e}")))
    }
}

/// Whether an algorithm is asymmetric — the only kind an `id_token` verified
/// against a published JWKS may use.
///
/// Written as an allowlist, not a blocklist. `jsonwebtoken::Algorithm` is
/// `#[non_exhaustive]`, so the wildcard is mandatory — and it must answer
/// `false`: an algorithm a future version of the library adds is one this
/// code has never reasoned about, and "which algorithms do we accept" is
/// exactly the question that must fail closed on an unknown answer.
fn is_asymmetric(alg: jsonwebtoken::Algorithm) -> bool {
    use jsonwebtoken::Algorithm::*;
    match alg {
        RS256 | RS384 | RS512 | ES256 | ES384 | PS256 | PS384 | PS512 | EdDSA => true,
        HS256 | HS384 | HS512 => false,
        _ => false,
    }
}

fn prune_pending(map: &mut HashMap<String, Pending>) {
    let ttl = std::time::Duration::from_secs(PENDING_TTL_SECS);
    map.retain(|_, p| p.created.elapsed() < ttl);
}

fn prune_sessions(map: &mut HashMap<String, Session>) {
    let idle = std::time::Duration::from_secs(SESSION_IDLE_SECS);
    let absolute = std::time::Duration::from_secs(SESSION_ABSOLUTE_SECS);
    map.retain(|_, s| s.last_seen.elapsed() < idle && s.created.elapsed() < absolute);
}

/// 256 bits of CSPRNG, base64url — used for state, nonce, PKCE verifier and
/// session id alike. All four need the same property: unguessable.
fn random_token() -> String {
    let mut raw = [0u8; 32];
    // A failure here means the OS has no entropy; there is no safe fallback,
    // and a predictable state/nonce/session id would be a silent auth bypass.
    getrandom::getrandom(&mut raw).expect("OS CSPRNG unavailable");
    b64url(&raw)
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest;
    b64url(&sha2::Sha256::digest(verifier.as_bytes()))
}

fn digest(s: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

/// Base64url without padding (RFC 4648 §5) — hand-rolled like the Basic-auth
/// decoder next door, for the same dependency-light reason.
fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Constant-time string compare for the nonce — a timing oracle on a replay
/// check is a small leak, but it is free to close.
fn ct_eq_str(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .into()
}

fn http_get(url: &str) -> Result<String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| AreevError::Validation(format!("GET {url}: {e}")))?
        .body_mut()
        .read_to_string()
        .map_err(|e| AreevError::Validation(format!("GET {url}: reading body: {e}")))
}

fn http_post_form(url: &str, form: &str) -> Result<String> {
    let resp = agent()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form);
    match resp {
        Ok(mut r) => r
            .body_mut()
            .read_to_string()
            .map_err(|e| AreevError::Validation(format!("POST {url}: reading body: {e}"))),
        // A 4xx carries the IdP's structured error, which the caller parses;
        // surface the body rather than only the status.
        Err(ureq::Error::StatusCode(code)) => Err(AreevError::Validation(format!(
            "POST {url}: HTTP {code}"
        ))),
        Err(e) => Err(AreevError::Validation(format!("POST {url}: {e}"))),
    }
}

fn fetch_jwks(url: &str) -> Result<jsonwebtoken::jwk::JwkSet> {
    let body = http_get(url)?;
    serde_json::from_str(&body)
        .map_err(|e| AreevError::Validation(format!("JWKS at {url}: malformed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PKCE S256 against RFC 7636's Appendix B vector — the one place a
    /// hand-rolled base64url could silently produce a challenge no IdP
    /// accepts (or, worse, one every IdP accepts from anybody).
    #[test]
    fn pkce_matches_the_rfc_7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(pkce_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn b64url_is_unpadded_and_url_safe() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        // The two characters that distinguish base64url from base64.
        assert_eq!(b64url(&[0xfb, 0xff, 0xfe]), "-__-");
        assert!(!b64url(&[0xff; 32]).contains('='));
    }

    #[test]
    fn random_tokens_are_unguessable_and_distinct() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43, "256 bits, unpadded base64url");
    }

    #[test]
    fn urlencode_escapes_what_would_break_a_query() {
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencode("https://x/cb"), "https%3A%2F%2Fx%2Fcb");
        assert_eq!(urlencode("-_.~"), "-_.~");
    }

    /// Sessions expire on BOTH clocks. Idle alone would let a stolen cookie
    /// live forever as long as the thief kept using it.
    #[test]
    fn sessions_prune_on_idle_and_absolute_age() {
        let mut map: HashMap<String, Session> = HashMap::new();
        let now = std::time::Instant::now();
        let old = now
            .checked_sub(std::time::Duration::from_secs(SESSION_ABSOLUTE_SECS + 1))
            .expect("test clock");
        // Actively used, but older than the absolute lifetime.
        map.insert("aged".into(), Session { principal: "p".into(), created: old, last_seen: now });
        // Young, but untouched past the idle window.
        let idle_since = now
            .checked_sub(std::time::Duration::from_secs(SESSION_IDLE_SECS + 1))
            .expect("test clock");
        map.insert(
            "idle".into(),
            Session { principal: "p".into(), created: now, last_seen: idle_since },
        );
        map.insert("live".into(), Session { principal: "p".into(), created: now, last_seen: now });

        prune_sessions(&mut map);
        assert!(!map.contains_key("aged"), "absolute lifetime must expire an active session");
        assert!(!map.contains_key("idle"), "idle timeout must expire an untouched session");
        assert!(map.contains_key("live"));
    }

    #[test]
    fn nonce_comparison_rejects_mismatches() {
        assert!(ct_eq_str("abc", "abc"));
        assert!(!ct_eq_str("abc", "abd"));
        assert!(!ct_eq_str("abc", "ab"));
        assert!(!ct_eq_str("", "x"));
    }

    /// Algorithm confusion (the classic JWT break): a token declaring
    /// `alg: HS256` and signed with the issuer's PUBLIC key as the HMAC
    /// secret verifies anywhere the verifier trusts the header. Symmetric
    /// algorithms are refused before a key is even selected.
    #[test]
    fn symmetric_algorithms_are_refused_and_unknown_ones_fail_closed() {
        use jsonwebtoken::Algorithm::*;
        for alg in [HS256, HS384, HS512] {
            assert!(!is_asymmetric(alg), "{alg:?} must never verify an id_token");
        }
        for alg in [RS256, RS384, RS512, ES256, ES384, PS256, PS384, PS512, EdDSA] {
            assert!(is_asymmetric(alg), "{alg:?} is a valid id_token algorithm");
        }
    }

    /// One login attempt, one redemption: the state is consumed on use, so a
    /// captured callback URL cannot be replayed into a second session.
    #[test]
    fn a_login_state_is_single_use() {
        let p = OidcProvider::with_test_session(test_cfg(), "sid", "user:a");
        let url = p.authorize_url().unwrap();
        let state = url
            .split("&state=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap()
            .to_string();

        // The pending entry exists exactly once.
        assert_eq!(p.pending.lock().unwrap().len(), 1);
        // Redeeming fails (no IdP here) but MUST still consume the state —
        // otherwise a failed exchange leaves a replayable attempt behind.
        let _ = p.complete_login("code", &state);
        assert_eq!(p.pending.lock().unwrap().len(), 0, "state must be consumed on use");

        // A second attempt with the same state is refused as unknown.
        let err = p.complete_login("code", &state).unwrap_err().to_string();
        assert!(err.contains("unknown or expired"), "{err}");
    }

    /// The authorize URL carries what RFC 9700 requires and nothing secret.
    #[test]
    fn the_authorize_url_is_pkce_s256_and_leaks_no_secret() {
        let p = OidcProvider::with_test_session(test_cfg(), "sid", "user:a");
        let url = p.authorize_url().unwrap();
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("code_challenge="), "{url}");
        assert!(url.contains("nonce="), "{url}");
        assert!(url.contains("state="), "{url}");
        assert!(url.contains("scope=openid"), "openid must be forced in: {url}");
        // The client secret and the PKCE *verifier* stay server-side; only
        // the challenge (a digest of the verifier) goes to the browser.
        assert!(!url.contains("s3cr3t"), "client secret must never reach the browser: {url}");
        let verifier = p.pending.lock().unwrap().values().next().unwrap().verifier.clone();
        assert!(!url.contains(&verifier), "the PKCE verifier must not be in the URL: {url}");
    }

    /// Logout invalidates SERVER-side. Clearing only the browser's copy would
    /// leave a cookie captured beforehand fully alive.
    #[test]
    fn logout_invalidates_the_session_server_side() {
        let p = OidcProvider::with_test_session(test_cfg(), "sid-1", "user:a");
        assert_eq!(p.principal_for_session("sid-1").as_deref(), Some("user:a"));
        p.logout("sid-1");
        assert!(p.principal_for_session("sid-1").is_none());
        // And an unknown id was never valid.
        assert!(p.principal_for_session("sid-2").is_none());
        assert!(p.principal_for_session("").is_none());
    }

    /// Sessions are stored under the DIGEST of their id, so a process memory
    /// dump yields nothing a browser could present.
    #[test]
    fn session_ids_are_not_stored_in_the_clear() {
        let p = OidcProvider::with_test_session(test_cfg(), "sid-secret", "user:a");
        let keys: Vec<String> = p.sessions.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 1);
        assert_ne!(keys[0], "sid-secret");
        assert_eq!(keys[0], digest("sid-secret"));
    }

    fn test_cfg() -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.example.com".into(),
            client_id: "console".into(),
            client_secret: "s3cr3t".into(),
            redirect_uri: "https://console.example.com/auth/callback".into(),
            scopes: "openid email".into(),
            principal_claim: "email".into(),
            principal_prefix: None,
        }
    }
}
