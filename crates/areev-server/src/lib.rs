//! areev-server — the opt-in thin HTTP surface and
//! the local test console (`areev ui`).
//!
//! Deliberately minimal: std-only HTTP/1.1 on 127.0.0.1, one request per
//! connection, JSON API + one embedded HTML page. This is a *local
//! inspection console*, not a service — no auth, binds loopback only.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use areev_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_loop_adapter::{now_ms, BorrowedSubstrate};
use serde_json::{json, Value};
use areev_loop::{Decision, Engine, RecStatus, RunOptions};

/// Native OIDC (A3) — the second recorded dependency-policy exception, behind
/// the non-default `oidc` feature. See the module docs for why a proxy
/// cannot cover the case this exists for.
#[cfg(feature = "oidc")]
pub mod oidc;

const CONSOLE_HTML: &str = include_str!("console.html");

/// Per-connection read/write timeout — bounds slow-client (slowloris) attacks.
const READ_TIMEOUT_SECS: u64 = 15;
/// Hard ceiling on bytes read from one connection (1 MiB body cap + headroom
/// for the request line and headers). Bounds memory against oversized requests.
const MAX_CONN_BYTES: u64 = (1 << 20) + (128 << 10);
/// Cap on total header bytes and header count per request.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
/// Total wall-clock deadline for reading + handling one request. A per-read
/// timeout cannot bound a slow-drip client (each byte resets it), so a watchdog
/// shuts the socket down at this deadline.
const REQUEST_DEADLINE_SECS: u64 = 30;
/// Issue #126: an auth-failure streak from one source IP idle longer than
/// this is treated as reset rather than accumulated against forever.
const AUTH_FAILURE_IDLE_SECS: u64 = 15 * 60;
/// Cap on distinct source IPs tracked for auth-failure throttling — the key
/// space is attacker-controlled, so this bounds the memory an attacker can
/// force the map to hold.
const MAX_AUTH_FAILURE_IPS: usize = 4096;
/// Longest accepted proxy-asserted identity. Principals are names, not
/// documents; anything longer is a malformed or hostile header, and the
/// string ends up in audit grains that replicate.
const MAX_SSO_IDENTITY_BYTES: usize = 128;
/// Principal names an SSO header may never assert.
///
/// `anonymous` is the restricted baseline and `user:console` is the owner
/// default the per-request binding restores on drop — an identity header
/// naming either would make audit records ambiguous between "a person the
/// proxy vouched for" and "the console's own unauthenticated floor/ceiling".
/// Neither is currently an escalation (`bind_principal` always builds a
/// *restricted* set from the file's grants), but a name that reads as a
/// system principal in an immutable, replicating audit trail is a defect
/// whether or not it is exploitable today.
const RESERVED_PRINCIPALS: [&str; 2] = ["anonymous", "user:console"];

/// Consecutive failed authentications from one source IP after which further
/// credential-bearing requests are refused with `429` until the streak goes
/// idle (`AUTH_FAILURE_IDLE_SECS`).
///
/// Set high enough that a human fat-fingering a pasted token a few times is
/// never locked out, and low enough that an online guessing attack gets a few
/// attempts per quarter-hour instead of thousands per second. It is a brake
/// on rate, not a replacement for token entropy — which is why `areev auth
/// mint` (256-bit tokens) is the real control and this is the backstop.
const MAX_CONSECUTIVE_AUTH_FAILURES: u32 = 10;


/// One accepted connection: plaintext, or TLS when the server was built
/// with the `tls` feature and configured with a certificate. The raw
/// socket handle stays available for timeouts and the watchdog shutdown;
/// all request/response bytes flow through this wrapper. A plaintext
/// client hitting a TLS listener fails the handshake — there is no
/// downgrade path by construction.
enum Conn {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            Conn::Tls(s) => s.flush(),
        }
    }
}

pub struct UiServer {
    facade: std::sync::Arc<AreevFacade>,
    executor: CalExecutor,
    db_label: String,
    /// `db_label`, redacted for DISPLAY (issue #124): every response body,
    /// the served HTML, and any startup log line must use this, never the
    /// raw label — on the Postgres backend `db_label` is a DSN with an
    /// inline password. Computed once in `UiServer::new` via `redact_dsn`.
    /// The raw `db_label` is kept only for the `resolve_for_memory` call
    /// sites below, which must keep comparing against the exact `--db`
    /// value a credential map's `memories` entry is written as.
    db_label_display: String,
    /// Shared secret required for access when set. It guards mutating
    /// endpoints (`Bearer`); in console-auth mode (`auth_all`) it guards
    /// every request via `Bearer` **or** HTTP `Basic` (password = token).
    token: Option<String>,
    /// When true, *every* request requires the token — the console page, all
    /// reads, and all writes — and a `401` carries `WWW-Authenticate: Basic` so
    /// browsers prompt. Set by [`UiServer::with_auth`].
    auth_all: bool,
    /// When false (the default), reject any request whose `Host` header is not
    /// loopback — the standard DNS-rebinding defense for a localhost service.
    /// Set true only when the operator intentionally serves to other hosts
    /// (CLI `--allow-remote`), where a non-loopback `Host` is expected.
    allow_remote: bool,
    /// Origins (issue #125), normalized, whose cross-origin **POST**s are
    /// accepted even though they are not loopback. The Origin drive-by
    /// check is NOT lifted by `allow_remote` — HTTP Basic is browser-cached
    /// and re-attached cross-site, so Origin is the only thing telling the
    /// console's own page apart from an attacker's page riding a viewer's
    /// cached credential. This is the exact-match allowlist an operator
    /// populates instead (CLI `--allow-origin`), one entry per
    /// [`allow_origin`](Self::allow_origin) call. No wildcards, no
    /// suffix/subdomain matching.
    allowed_origins: Vec<String>,
    /// Host loop policy (§6.2) applied to the `/api/loop/*` engine — the
    /// same `loop-policy.json` the CLI takes (`areev ui --policy`). Absent →
    /// the closed default: nothing auto-applies, nothing is denied. Host
    /// config, never persisted in the memory file.
    loop_policy: Option<areev_loop::Policy>,
    /// Multi-principal mode (`areev ui --auth areev-auth.json`): a credential
    /// map resolving bearer/Basic tokens to principal names. Rights come
    /// from the FILE's own grant grains — the map holds no policy and no
    /// raw secrets. Unauthenticated requests run as `anonymous`
    /// (read-only unless the file grants more). It changes *who* a request
    /// is, never *whether* the shared `--token-env` secret is required: when
    /// both are configured the secret keeps its contract (implied admin, and
    /// still demanded on the requests it guards).
    credentials: Option<areev_core::authz::CredentialMap>,
    /// Whether destructive CAL is permitted, kept alongside the executor so
    /// every builder can rebuild it without clobbering the others' settings.
    allow_destructive_ops: bool,
    /// Native TLS (the recorded dependency exception; `tls` cargo feature).
    /// None = plaintext — the documented default is a TLS-terminating proxy.
    #[cfg(feature = "tls")]
    tls_config: Option<std::sync::Arc<rustls::ServerConfig>>,
    /// SSO v0 — trusted-header auth (`areev ui --sso-header X-Forwarded-User
    /// --sso-secret-env VAR`): an authenticating proxy does OIDC/SAML and
    /// forwards the identity in this header. Honored ONLY when the request
    /// also carries the proxy shared secret in `x-areev-proxy-secret` —
    /// otherwise identity headers are attacker-controlled input and are
    /// ignored entirely. Rights still come from the FILE's grant grains.
    sso_header: Option<String>,
    /// The proxy shared secrets this instance accepts, newest first.
    ///
    /// A list rather than a value so the secret can be **rotated without a
    /// zero-overlap cutover** (#79). It is an impersonation-grade credential:
    /// whoever holds it can present any identity header, approval-capable
    /// principals included. Rotating one of those atomically across a proxy
    /// fleet and a console is not achievable in practice, so the honest
    /// choices were a brief outage or a window where the wrong secret is live
    /// — and an operator facing either under suspected-compromise pressure
    /// will defer the rotation, which is the outcome that actually costs.
    ///
    /// Two at a time, deliberately: enough for old-and-new, not enough to
    /// accumulate a drawer of forgotten live credentials. Both are checked in
    /// constant time and the comparison never short-circuits on the first
    /// mismatch, so timing cannot reveal which one matched or how many are
    /// configured.
    sso_secrets: Vec<String>,
    /// Optional groups header (A2, `--sso-groups-header X-Forwarded-Groups`):
    /// a comma-separated list of IdP groups, honored under the same proxy
    /// secret as `sso_header`. Resolved through the credential map's `groups`
    /// table, and only when the asserted identity has no grants of its own —
    /// see `resolve_sso_principal` for the precedence and why.
    sso_groups_header: Option<String>,
    /// Prefix stamped onto every proxy-asserted principal
    /// (`--sso-principal-prefix`). Lets an operator keep IdP-sourced
    /// identities visibly distinct from credential-map ones in grant grains
    /// and audit records, so `GRANT` can target one population without
    /// depending on IdP naming conventions never colliding with local ones.
    sso_principal_prefix: Option<String>,
    /// Native OIDC (A3, `oidc` feature). When configured, the console serves
    /// its own login: `/auth/login` → the IdP → `/auth/callback` → an
    /// `HttpOnly` session cookie. Unlike the trusted-header path, the
    /// identity is proven by a signature over an issuer-published key set,
    /// which is why an OIDC principal MAY approve by default — that
    /// difference is the entire reason the feature exists.
    #[cfg(feature = "oidc")]
    oidc: Option<std::sync::Arc<oidc::OidcProvider>>,
    /// Whether a proxy-asserted SSO identity may answer a HITL approval
    /// (`POST /api/run/respond`). **Default false — deny.**
    ///
    /// `run.respond` already refuses shared-token and anonymous callers
    /// because the approver's identity IS the audit record. An SSO identity
    /// arrives in a header, trusted because the request also carried
    /// `sso_secrets` — a static, shared, impersonation-grade value. Whoever
    /// holds it can assert *any* identity, approval-capable principals
    /// included, and the resulting audit grain is indistinguishable from a
    /// genuine approval: a well-formed answer by a granted principal. The
    /// blast radius of that one secret is the integrity of the whole HITL
    /// trail — which is the control the governance story rests on.
    ///
    /// A credential-map principal is materially stronger (it holds a
    /// per-principal secret, not a fleet-wide one) and is unaffected by this.
    /// So the default fails closed and an operator who accepts the trade-off
    /// opts in explicitly with `areev ui --sso-approvals allow`.
    sso_approvals: bool,
    /// Auth-failure counter (issue #126), keyed by source IP — the port is
    /// deliberately excluded, since a NAT'd or proxied attacker's source
    /// port varies per connection while the IP does not. Each entry is
    /// `(consecutive failures, last-failure time)`; see
    /// `note_auth_failure`/`reset_auth_failures`. The map is
    /// attacker-controlled key space (one entry per source IP that ever
    /// fails once) and is bounded at `MAX_AUTH_FAILURE_IPS` for exactly that
    /// reason. The lock is held only for the map bookkeeping itself, never
    /// across a response write.
    ///
    /// This counts and logs — it does **not** delay or lock out. See
    /// `note_auth_failure` for why: `serve` is a strictly serial accept
    /// loop, so any per-request sleep here would be a lever an
    /// unauthenticated caller pulls to stall the console for everyone, not
    /// just themselves.
    auth_failures: std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
}

/// A login-flow error, rendered as text rather than JSON: the audience is a
/// browser mid-redirect, not a script. The message is ours, never the IdP's
/// raw response body.
#[cfg(feature = "oidc")]
fn auth_error(msg: &str) -> (String, String, String) {
    (
        "400 Bad Request".into(),
        "Content-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\n".into(),
        format!("login failed: {msg}\n\nStart again at /auth/login\n"),
    )
}

#[cfg(feature = "oidc")]
fn not_found_auth() -> (String, String, String) {
    (
        "404 Not Found".into(),
        "Content-Type: text/plain; charset=utf-8\r\n".into(),
        "no such auth endpoint\n".into(),
    )
}

/// One cookie's value out of a `Cookie:` header, or `None`.
///
/// Deliberately exact on the name: a `Cookie` header is attacker-influenced
/// (any script on any same-site origin can set cookies), so a prefix match
/// would let `areev_session_decoy=...` shadow the real one.
#[cfg(feature = "oidc")]
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// Validate a proxy-asserted identity before it becomes a principal.
///
/// The proxy secret decides *who may assert*; it says nothing about whether
/// what was asserted is well-formed. An identity flows into audit grains that
/// are immutable and replicate, so a control character here is a log-injection
/// that outlives the request — and a name colliding with a system principal is
/// an ambiguity no later reader can resolve.
///
/// Returns `None` for anything rejected, which the caller treats exactly like
/// an absent header: the request proceeds as whatever its other credentials
/// make it. Refusing the *request* would let a misconfigured proxy take the
/// console down; refusing the *identity* fails closed on rights instead.
fn sanitize_sso_identity(raw: &str) -> Option<String> {
    let id = raw.trim();
    if id.is_empty() || id.len() > MAX_SSO_IDENTITY_BYTES {
        return None;
    }
    // No control characters (CR/LF above all — header smuggling into logs),
    // and no internal whitespace: a principal is one token, and " user:a"
    // vs "user:a" must not be two audit identities for one person.
    if id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    if RESERVED_PRINCIPALS.contains(&id) {
        return None;
    }
    Some(id.to_string())
}

/// Per-request principal binding. The server handles one request at a
/// time, so rebinding the shared facade for the request's duration is
/// race-free; the guard restores the owner default on drop — including on
/// panic — so a failed handler can never leak a stray principal into the
/// next request.
struct RequestBinding<'a> {
    facade: &'a AreevFacade,
}

impl<'a> RequestBinding<'a> {
    fn bind(
        facade: &'a AreevFacade,
        map: &areev_core::authz::CredentialMap,
        bearer: Option<&str>,
        memory: &str,
    ) -> Self {
        // Memory-scoped resolution: a credential whose `memories` list
        // excludes THIS memory is indistinguishable from an unknown token.
        let principal =
            bearer.and_then(|t| map.resolve_for_memory(t, memory).ok().map(str::to_string));
        match principal {
            Some(p) => {
                if facade.bind_principal(&p).is_err() {
                    // A store failure reading grants fails toward LESS
                    // privilege, never more.
                    Self::bind_anonymous(facade);
                }
            }
            None => Self::bind_anonymous(facade),
        }
        RequestBinding { facade }
    }

    /// `anonymous` = a read-only baseline plus whatever the file explicitly
    /// grants the `anonymous` principal — the formalized version of the old
    /// token-less read-only console.
    /// Bind a proxy-proven SSO identity directly (no credential map row
    /// needed — the IdP said who this is; the FILE says what they may do).
    fn bind_identity(facade: &'a AreevFacade, principal: &str) -> Self {
        if facade.bind_principal(principal).is_err() {
            Self::bind_anonymous(facade);
        }
        RequestBinding { facade }
    }

    fn bind_anonymous(facade: &AreevFacade) {
        use areev_core::authz::{AuthzSet, Grant, Verb};
        let mut grants = facade
            .with_store(|m| m.authz_grants("anonymous"))
            .unwrap_or_default();
        grants.push(Grant { verbs: vec![Verb::Read], namespaces: vec!["*".to_string()] });
        facade.bind(AuthzSet::restricted("anonymous", grants));
    }
}

impl Drop for RequestBinding<'_> {
    fn drop(&mut self) {
        self.facade
            .bind(areev_core::authz::AuthzSet::owner("user:console"));
    }
}

impl UiServer {
    pub fn new(facade: AreevFacade, db_label: String) -> Self {
        let db_label_display = redact_dsn(&db_label);
        UiServer {
            facade: std::sync::Arc::new(facade),
            executor: CalExecutor::new(CalExecutorConfig::default())
                .with_governance(std::sync::Arc::new(areev_loop_adapter::LoopGovernance::new())),
            db_label,
            db_label_display,
            token: None,
            auth_all: false,
            allow_remote: false,
            allowed_origins: Vec::new(),
            loop_policy: None,
            credentials: None,
            allow_destructive_ops: CalExecutorConfig::default().allow_destructive_ops,
            #[cfg(feature = "tls")]
            tls_config: None,
            sso_header: None,
            sso_secrets: Vec::new(),
            sso_groups_header: None,
            sso_principal_prefix: None,
            #[cfg(feature = "oidc")]
            oidc: None,
            sso_approvals: false,
            auth_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Enable trusted-header SSO (v0). `header` names the identity header
    /// the authenticating proxy forwards; `secret` is the proxy's shared
    /// secret, demanded in `x-areev-proxy-secret` on every SSO request.
    pub fn with_sso(self, header: impl Into<String>, secret: impl Into<String>) -> Self {
        self.with_sso_rotating(header, secret, None::<String>)
    }

    /// [`with_sso`](Self::with_sso) with a second secret accepted during a
    /// rotation window (#79).
    ///
    /// `next` is the incoming secret; `secret` stays the one already deployed.
    /// While both are configured either proves the proxy, so the fleet can be
    /// moved over one node at a time and the old secret retired afterwards —
    /// the way a TLS key is rotated. Order carries no privilege: they are
    /// equally valid until one is removed, which is what makes the window
    /// safe to run in both directions (roll forward, or roll back).
    ///
    /// The window is meant to be short. Nothing here enforces that — a
    /// deadline the server could enforce would have to live in the file or a
    /// clock it does not own — so retiring the old secret is the operator's
    /// step, and `docs/runbooks/sso-secret-rotation.md` is the procedure.
    pub fn with_sso_rotating(
        mut self,
        header: impl Into<String>,
        secret: impl Into<String>,
        next: Option<impl Into<String>>,
    ) -> Self {
        let header = header.into();
        let secret = secret.into();
        let next = next.map(Into::into).filter(|s| !s.trim().is_empty());
        // An empty proxy secret would let `x-areev-proxy-secret:` (empty)
        // prove any identity — refuse loudly at the library boundary too,
        // not just in the CLI.
        assert!(
            !header.trim().is_empty() && !secret.trim().is_empty(),
            "with_sso requires a non-empty header name and proxy secret"
        );
        // A rotation that deploys the same value twice is a rotation that did
        // not happen, and it would read as "both live" in every log and
        // status line. Refuse rather than silently accept the no-op.
        assert!(
            next.as_deref() != Some(secret.as_str()),
            "the rotating proxy secret must differ from the current one"
        );
        self.sso_header = Some(header.to_ascii_lowercase());
        self.sso_secrets = match next {
            Some(n) => vec![secret, n],
            None => vec![secret],
        };
        self
    }

    /// Serve native OIDC login for the console (A3).
    #[cfg(feature = "oidc")]
    pub fn with_oidc(mut self, provider: oidc::OidcProvider) -> Self {
        self.oidc = Some(std::sync::Arc::new(provider));
        self
    }

    /// The principal for this request's session cookie, if any.
    #[cfg(feature = "oidc")]
    fn oidc_principal(&self, cookie_header: Option<&str>) -> Option<String> {
        let provider = self.oidc.as_ref()?;
        let sid = cookie_value(cookie_header?, oidc::SESSION_COOKIE)?;
        provider.principal_for_session(&sid)
    }

    /// The `/auth/*` endpoints (A3). Returns `(status, extra headers, body)`.
    ///
    /// These are the only routes that set headers of their own, which is why
    /// they bypass `route_full`'s (status, content-type, body) contract.
    #[cfg(feature = "oidc")]
    fn oidc_route(
        &self,
        method: &str,
        path: &str,
        cookie_header: Option<&str>,
    ) -> (String, String, String) {
        let provider = match &self.oidc {
            Some(p) => p,
            None => return not_found_auth(),
        };
        let (base, query) = match path.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path, ""),
        };
        let q = |key: &str| -> Option<String> {
            query.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == key).then(|| urldecode(v))
            })
        };
        // `Secure` only when the redirect URI is https: a loopback console
        // over plain http must still be able to log in, and a `Secure` cookie
        // the browser refuses to send would look like a broken login rather
        // than a policy.
        let secure = if provider.is_secure() { "; Secure" } else { "" };

        match (method, base) {
            ("GET", "/auth/login") => match provider.authorize_url() {
                Ok(url) => (
                    "302 Found".into(),
                    format!("Location: {url}\r\nCache-Control: no-store\r\n"),
                    String::new(),
                ),
                Err(e) => auth_error(&e.to_string()),
            },
            ("GET", "/auth/callback") => {
                let (Some(code), Some(state)) = (q("code"), q("state")) else {
                    // An IdP-side failure comes back as ?error=...; surface
                    // it rather than a bare "missing code".
                    return auth_error(&match q("error") {
                        Some(e) => format!("the identity provider refused the login: {e}"),
                        None => "callback missing `code` or `state`".to_string(),
                    });
                };
                match provider.complete_login(&code, &state) {
                    Ok(sid) => (
                        "302 Found".into(),
                        format!(
                            "Location: /\r\nCache-Control: no-store\r\n\
                             Set-Cookie: {}={sid}; HttpOnly; SameSite=Strict; Path=/{secure}\r\n",
                            oidc::SESSION_COOKIE
                        ),
                        String::new(),
                    ),
                    Err(e) => auth_error(&e.to_string()),
                }
            }
            // Logout accepts POST (the console's button, Origin-checked like
            // every other mutation) and GET (a bookmarkable escape hatch —
            // and a GET logout is not a CSRF worth defending: forcing someone
            // to log out grants an attacker nothing).
            ("POST", "/auth/logout") | ("GET", "/auth/logout") => {
                if let Some(sid) = cookie_header.and_then(|c| cookie_value(c, oidc::SESSION_COOKIE))
                {
                    // Invalidate SERVER-side too. Clearing only the browser's
                    // copy would leave a cookie captured beforehand alive.
                    provider.logout(&sid);
                }
                (
                    "302 Found".into(),
                    format!(
                        "Location: /\r\nCache-Control: no-store\r\n\
                         Set-Cookie: {}=; HttpOnly; SameSite=Strict; Path=/{secure}; Max-Age=0\r\n",
                        oidc::SESSION_COOKIE
                    ),
                    String::new(),
                )
            }
            _ => not_found_auth(),
        }
    }

    /// Honor an IdP groups header (A2) under the same proxy secret as the
    /// identity header. Groups resolve through the credential map's `groups`
    /// table; without a map, this does nothing.
    pub fn with_sso_groups_header(mut self, header: impl Into<String>) -> Self {
        let header = header.into();
        assert!(
            !header.trim().is_empty(),
            "with_sso_groups_header requires a non-empty header name"
        );
        self.sso_groups_header = Some(header.to_ascii_lowercase());
        self
    }

    /// Stamp `prefix` onto every proxy-asserted principal, so IdP-sourced
    /// identities are visibly distinct from credential-map ones everywhere
    /// they land — grants, logs, audit grains.
    pub fn with_sso_principal_prefix(mut self, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        assert!(
            !prefix.trim().is_empty(),
            "with_sso_principal_prefix requires a non-empty prefix"
        );
        self.sso_principal_prefix = Some(prefix);
        self
    }

    /// Resolve a proxy-proven request to its principal (A2).
    ///
    /// Precedence, most specific first:
    /// 1. the asserted **identity**, when the file grants it anything;
    /// 2. the first **group** (in header order) the credential map maps to a
    ///    principal;
    /// 3. nothing — the request falls through to `anonymous`.
    ///
    /// Identity-before-group is what makes an individual grant able to
    /// override a role, which is the direction an operator expects (and the
    /// direction that lets someone be *removed* from an exception without
    /// editing the group). Groups are consulted only when the identity is
    /// ungranted, so adding a groups header can never *narrow* a person who
    /// already had rights.
    ///
    /// Returns `(principal, from_group)`. `from_group` matters downstream: a
    /// role is not a person, so a group-derived principal can never answer an
    /// approval — "someone in engineering approved this" is not an audit
    /// record, whatever `--sso-approvals` says.
    fn resolve_sso_principal(
        &self,
        identity: &str,
        groups_raw: Option<&str>,
    ) -> Option<(String, bool)> {
        let stamp = |p: &str| match &self.sso_principal_prefix {
            Some(prefix) if !p.starts_with(prefix.as_str()) => format!("{prefix}{p}"),
            _ => p.to_string(),
        };
        let identity = stamp(identity);

        // "Has grants of its own" is asked of the FILE, which is the only
        // authority on rights. A store error answers "no" — falling toward
        // the group (and ultimately anonymous) rather than assuming rights
        // we could not read.
        let identity_granted = self
            .facade
            .with_store(|m| m.authz_grants(&identity))
            .map(|g| !g.is_empty())
            .unwrap_or(false);
        if identity_granted {
            return Some((identity, false));
        }

        if let (Some(map), Some(raw)) = (&self.credentials, groups_raw) {
            for group in raw.split(',') {
                if let Some(principal) = map.principal_for_group(group) {
                    return Some((principal.to_string(), true));
                }
            }
        }

        // No grants either way: still bind the identity. It is who the proxy
        // says this is, `bind_principal` resolves it to the same empty grant
        // set anonymous would get, and an audit line naming the person beats
        // one naming "anonymous".
        Some((identity, false))
    }

    /// Permit proxy-asserted SSO identities to answer HITL approvals
    /// (`POST /api/run/respond`). **Off by default**: the identity header's
    /// only proof is the shared, fleet-wide proxy secret, so whoever holds it
    /// could approve as anyone — including the approver the audit record then
    /// names.
    ///
    /// Turning this on is a deliberate statement that the proxy shared secret
    /// is held to the same standard as an approver's own credential. That is
    /// achievable (a proxy on a Unix socket, or mTLS between proxy and
    /// console, with the secret never leaving the host) — it is just not
    /// something the console can verify, so it cannot be the default.
    pub fn with_sso_approvals(mut self, allow: bool) -> Self {
        self.sso_approvals = allow;
        self
    }

    /// Rebuild the executor from the current gate settings. Every builder
    /// that touches the CAL surface goes through here so `--no-destructive-ops`
    /// and `--policy` compose instead of clobbering each other's config —
    /// and the governance host survives every rebuild. (Same shape as
    /// `areev-mcp`'s; building `CalExecutorConfig::default()` in one builder
    /// silently re-enabled destruction configured off by another.)
    fn rebuild_executor(&mut self) {
        self.executor = CalExecutor::new(CalExecutorConfig {
            allow_destructive_ops: self.allow_destructive_ops,
            ..CalExecutorConfig::default()
        })
        .with_governance(std::sync::Arc::new(match &self.loop_policy {
            Some(p) => areev_loop_adapter::LoopGovernance::with_policy(p.clone()),
            None => areev_loop_adapter::LoopGovernance::new(),
        }));
    }

    /// Multi-principal mode: attach the credential map (`areev-auth.json`).
    /// Tokens resolve to principal names; rights come from the memory
    /// file's own grant grains; unauthenticated requests run as
    /// `anonymous`. See `docs/cal-reference.md` §9.
    pub fn with_credentials(mut self, map: areev_core::authz::CredentialMap) -> Self {
        self.credentials = Some(map);
        self
    }

    /// Attach a host loop policy to the console's loop routes, so a
    /// console-triggered run honors the same auto-apply grants, denies, and
    /// severity floors as `areev loop run --policy`.
    pub fn with_loop_policy(mut self, policy: areev_loop::Policy) -> Self {
        self.loop_policy = Some(policy);
        // The CAL surface's governance host honors the same policy.
        self.rebuild_executor();
        self
    }

    /// The loop engine for a request: builtins + the host policy when one
    /// was attached.
    fn engine(&self) -> Engine {
        match &self.loop_policy {
            Some(p) => Engine::with_builtins().with_policy(p.clone()),
            None => Engine::with_builtins(),
        }
    }

    /// Accept requests whose `Host` header is non-loopback. Off by default so
    /// the loopback console is protected against DNS-rebinding reads even when
    /// unauthenticated; the CLI enables it under `--allow-remote`.
    pub fn allow_remote(mut self, yes: bool) -> Self {
        self.allow_remote = yes;
        self
    }

    /// Accept a cross-origin **POST** whose `Origin` header exactly matches
    /// this origin (scheme + host[:port], compared case-insensitively with a
    /// single trailing `/` ignored). Call once per origin to allow — there
    /// is no wildcard or subdomain form, by design: naming
    /// `https://console.example.com` must never thereby accept
    /// `https://evil-console.example.com`.
    ///
    /// `--allow-remote` does **not** imply this. The Origin check exists
    /// because the console authenticates browsers with HTTP Basic, which the
    /// browser caches and re-attaches to cross-site requests — Origin is the
    /// only thing telling the console's own page apart from an attacker's
    /// page riding a viewer's cached credential, so it is CSRF protection,
    /// not defence in depth, and it stays enforced on every non-loopback
    /// origin except the ones named here.
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_origins.push(normalize_origin(&origin.into()));
        self
    }

    /// Whether `origin` (a request's raw `Origin` header value) matches one
    /// of the operator-configured [`allow_origin`](Self::allow_origin)
    /// entries, after normalizing both sides identically. Exact match only.
    fn origin_is_allowed(&self, origin: &str) -> bool {
        let normalized = normalize_origin(origin);
        self.allowed_origins.contains(&normalized)
    }

    /// Require a shared secret on **every** request (the console page, reads,
    /// and writes). Browsers authenticate through the native `Basic` prompt
    /// (any username; password = `token`); scripts may send `Authorization:
    /// Bearer <token>`. Use this to serve the console to more than a single
    /// trusted local operator. Pair with a TLS-terminating proxy for non-local
    /// exposure — the token crosses the wire in the clear otherwise.
    pub fn with_auth(mut self, token: String) -> Self {
        // An empty token would authenticate `Authorization: Bearer ` (the
        // empty credential) as the OWNER — refuse at the library boundary,
        // not just in the CLI's --token-env validation.
        assert!(
            !token.trim().is_empty(),
            "with_auth requires a non-empty token"
        );
        self.token = Some(token);
        self.auth_all = true;
        self
    }

    /// Permit or forbid destructive CAL (`FORGET <hash>`) from the query
    /// console. Enabled by default; pass `false` to serve a read-only console.
    /// Since the plain console is unauthenticated, disabling this is the safe
    /// choice when the console is exposed beyond a trusted local operator.
    pub fn allow_destructive_ops(mut self, allow: bool) -> Self {
        self.allow_destructive_ops = allow;
        self.rebuild_executor();
        self
    }

    /// Bind and return the listener (lets callers learn the ephemeral port).
    pub fn bind(addr: &str) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr)
    }

    /// Serve forever on an already-bound listener.
    pub fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.handle(s) {
                        eprintln!("areev ui: request error: {e}");
                    }
                }
                Err(e) => eprintln!("areev ui: accept error: {e}"),
            }
        }
        Ok(())
    }

    /// Configure native TLS from PEM files (`tls` cargo feature). The
    /// documented default remains a TLS-terminating proxy; this is for
    /// deployments with nowhere to put one (edge boxes, appliances).
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, cert_pem: &str, key_pem: &str) -> std::io::Result<Self> {
        use rustls::pki_types::pem::PemObject;
        let pem_err =
            |e: rustls::pki_types::pem::Error| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string());
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            rustls::pki_types::CertificateDer::pem_file_iter(cert_pem)
                .map_err(pem_err)?
                .collect::<Result<_, _>>()
                .map_err(pem_err)?;
        let key =
            rustls::pki_types::PrivateKeyDer::from_pem_file(key_pem).map_err(pem_err)?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        self.tls_config = Some(std::sync::Arc::new(config));
        Ok(self)
    }

    /// Record one failed authentication attempt from `ip` (issue #126) and
    /// return the new consecutive-failure count — for the caller to LOG,
    /// never to sleep on. An entry idle for more than
    /// `AUTH_FAILURE_IDLE_SECS` is treated as reset rather than accumulating
    /// forever. The map is bounded at `MAX_AUTH_FAILURE_IPS` — the key is
    /// one attacker-controlled source IP per entry, so an unbounded map here
    /// would itself be a memory-exhaustion vector; past the cap, the
    /// stalest entry is evicted before the new one is inserted. The lock is
    /// held only for this bookkeeping — never across the response write.
    ///
    /// **Deliberately no artificial delay or lockout on this count.**
    /// `UiServer::serve` is a strictly SERIAL accept loop — one connection
    /// handled at a time, per its own doc comment — so a `std::thread::sleep`
    /// here, before a response is written, would not slow down one
    /// attacker: it would stall the entire console for every caller,
    /// including the operator, behind whatever backoff an unauthenticated
    /// caller chooses to trigger by sending bad credentials. That trade is
    /// worse than the problem this counter exists to fix (a 401 that reads
    /// as ordinary traffic in the log). Rate limiting belongs in front of
    /// this server — the TLS-terminating proxy the deployment profile
    /// already calls for — where it can also see the caller's real address;
    /// this count and the log line built from it are what such a rule is
    /// written against. Do not reintroduce a sleep/lockout here without
    /// first making the accept loop concurrent.
    fn note_auth_failure(&self, ip: std::net::IpAddr) -> u32 {
        let now = std::time::Instant::now();
        let idle = Duration::from_secs(AUTH_FAILURE_IDLE_SECS);
        let mut map = self
            .auth_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get_mut(&ip) {
            Some((n, last)) if now.duration_since(*last) < idle => {
                *n = n.saturating_add(1);
                *last = now;
                *n
            }
            Some(entry) => {
                *entry = (1, now);
                1
            }
            None => {
                if map.len() >= MAX_AUTH_FAILURE_IPS {
                    if let Some(stalest) =
                        map.iter().min_by_key(|(_, (_, last))| *last).map(|(k, _)| *k)
                    {
                        map.remove(&stalest);
                    }
                }
                map.insert(ip, (1, now));
                1
            }
        }
    }

    /// Clear a source IP's failure streak after it authenticates successfully.
    fn reset_auth_failures(&self, ip: std::net::IpAddr) {
        let mut map = self
            .auth_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(&ip);
    }

    /// A source IP's current consecutive-failure count (0 if it has none, or
    /// if its last failure is older than `AUTH_FAILURE_IDLE_SECS`).
    ///
    /// The idle check is applied on READ as well as on write so a lockout
    /// expires on its own: a stale entry that `note_auth_failure` has not
    /// been called on again must not keep refusing forever, which is what
    /// turns a brute-force brake into a self-inflicted outage.
    fn auth_failure_count(&self, ip: std::net::IpAddr) -> u32 {
        let idle = Duration::from_secs(AUTH_FAILURE_IDLE_SECS);
        self.auth_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&ip)
            .filter(|(_, last)| last.elapsed() < idle)
            .map(|(n, _)| *n)
            .unwrap_or(0)
    }

    /// Test-only alias — tests read the counter directly because there is no
    /// sleep to observe via timing (see `note_auth_failure`).
    #[cfg(test)]
    fn auth_failure_count_for_test(&self, ip: std::net::IpAddr) -> u32 {
        self.auth_failure_count(ip)
    }

    /// Wrap an accepted socket per the TLS posture. The handshake happens
    /// lazily on first read/write; a plaintext client fails it loudly.
    fn wrap(&self, stream: TcpStream) -> std::io::Result<Conn> {
        #[cfg(feature = "tls")]
        if let Some(config) = &self.tls_config {
            let session = rustls::ServerConnection::new(std::sync::Arc::clone(config))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            return Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(session, stream))));
        }
        Ok(Conn::Plain(stream))
    }

    fn handle(&self, stream: TcpStream) -> std::io::Result<()> {
        // The server handles one connection at a time, so a single slow-drip
        // client could otherwise hold it open indefinitely (a per-read timeout
        // resets on every byte). A watchdog thread enforces a hard wall-clock
        // deadline by shutting the socket down; it is unparked and joined the
        // instant the request completes, so it adds no latency in the fast path.
        let watchdog_stream = stream.try_clone()?;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog_done = std::sync::Arc::clone(&done);
        let watchdog = std::thread::spawn(move || {
            std::thread::park_timeout(Duration::from_secs(REQUEST_DEADLINE_SECS));
            if !watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
                let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
            }
        });
        let raw = stream.try_clone()?;
        let conn = self.wrap(stream)?;
        let result = self.handle_request(&raw, conn);
        done.store(true, std::sync::atomic::Ordering::Release);
        watchdog.thread().unpark();
        let _ = watchdog.join();
        result
    }

    fn handle_request(&self, raw: &TcpStream, mut conn: Conn) -> std::io::Result<()> {
        // Bound slow clients (slowloris) and total bytes read from the
        // connection so a malicious client cannot stall the server or exhaust
        // memory with an oversized request line / headers / body. The
        // timeouts sit on the RAW socket, so they bound TLS handshakes too.
        raw.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
        raw.set_write_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
        let mut reader = BufReader::new((&mut conn).take(MAX_CONN_BYTES));
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("/").to_string();

        // headers → content-length + authorization + origin + host
        let mut content_length = 0usize;
        let mut bearer: Option<String> = None;
        let mut origin: Option<String> = None;
        let mut host: Option<String> = None;
        let mut sso_identity_raw: Option<String> = None;
        let mut sso_groups_raw: Option<String> = None;
        #[cfg(feature = "oidc")]
        let mut cookie_header: Option<String> = None;
        let mut proxy_secret: Option<String> = None;
        let mut header_bytes = 0usize;
        let mut header_count = 0usize;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // EOF / connection closed by peer
            }
            header_bytes += n;
            header_count += 1;
            if header_bytes > MAX_HEADER_BYTES || header_count > MAX_HEADERS {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request headers too large",
                ));
            }
            let l = line.trim();
            if l.is_empty() {
                break;
            }
            let low = l.to_ascii_lowercase();
            if let Some(v) = low.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if let Some(v) = low.strip_prefix("origin:") {
                origin = Some(v.trim().to_string());
            }
            if let Some(v) = low.strip_prefix("host:") {
                host = Some(v.trim().to_string());
            }
            #[cfg(feature = "oidc")]
            if low.starts_with("cookie:") {
                // Re-slice the ORIGINAL line: the lowercased copy would
                // mangle a base64url session id.
                if let Some((_, orig)) = l.split_once(':') {
                    cookie_header = Some(orig.trim().to_string());
                }
            }
            if let Some(v) = low.strip_prefix("x-areev-proxy-secret:") {
                // Secrets are compared, never logged; the lowercased copy is
                // wrong for comparison, so re-slice the original line.
                let _ = v;
                if let Some((_, orig)) = l.split_once(':') {
                    proxy_secret = Some(orig.trim().to_string());
                }
            }
            if let Some(h) = &self.sso_header {
                if low.starts_with(&format!("{h}:")) {
                    if let Some((_, orig)) = l.split_once(':') {
                        sso_identity_raw = Some(orig.trim().to_string());
                    }
                }
            }
            if let Some(h) = &self.sso_groups_header {
                if low.starts_with(&format!("{h}:")) {
                    if let Some((_, orig)) = l.split_once(':') {
                        sso_groups_raw = Some(orig.trim().to_string());
                    }
                }
            }
            if low.starts_with("authorization:") {
                if let Some((_, v)) = l.split_once(':') {
                    let v = v.trim();
                    // Accept `Bearer <token>` (scripts/CLI) or HTTP `Basic`
                    // (browsers, via the native login prompt). For Basic the
                    // credential is base64(user:pass) and the token is the
                    // password; the username is ignored.
                    bearer = if let Some(t) =
                        v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))
                    {
                        Some(t.trim().to_string())
                    } else if let Some(b64) =
                        v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))
                    {
                        basic_auth_password(b64.trim())
                    } else {
                        Some(v.to_string())
                    };
                }
            }
        }
        // DNS-rebinding defense: a browser tricked into pointing an attacker
        // domain at 127.0.0.1 sends that domain in the Host header. Unless the
        // operator opted into remote serving, reject any non-loopback Host on
        // *every* method (the Origin check below only covers POST, so this is
        // what stops a drive-by page from reading memory over GET). A missing
        // Host (bare HTTP/1.0, some CLI clients) passes, mirroring Origin.
        if !self.allow_remote && host.as_deref().is_some_and(|h| !host_is_local(h)) {
            let payload = br#"{"ok":false,"error":"non-loopback Host rejected (use --allow-remote to serve remotely)"}"#;
            drop(reader);
            let mut out = conn;
            write!(
                out,
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            out.write_all(payload)?;
            return out.flush();
        }

        let mut body = vec![0u8; content_length.min(1 << 20)];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }

        // Cross-origin drive-by protection: browsers attach an Origin header
        // to cross-site requests. Only loopback pages (the console itself),
        // or an origin the operator explicitly allowlisted with
        // --allow-origin (issue #125), may mutate; curl/CLI clients send no
        // Origin and pass through. `--allow-remote` does NOT lift this on
        // its own — see `allow_origin`'s doc comment for why.
        if method == "POST"
            && origin
                .as_deref()
                .is_some_and(|o| !origin_is_local(o) && !self.origin_is_allowed(o))
        {
            let payload = br#"{"ok":false,"error":"cross-origin request rejected"}"#;
            drop(reader);
            let mut out = conn;
            write!(
                out,
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            out.write_all(payload)?;
            return out.flush();
        }

        // Captured before the move below: issue #126's failure counter needs
        // to know whether a proxy secret was PRESENTED at all (a
        // wrong-guess attempt to count), independent of `sso_identity`,
        // which by then only ever reflects a *proven* identity.
        let proxy_secret_presented = proxy_secret.is_some();
        // The identity header is TRUSTED only when the proxy proved itself
        // (constant-time secret check). A forged header without the secret
        // is silently ignored — the request proceeds as whatever its other
        // credentials make it.
        //
        // [A2] The secret proves the PROXY; it says nothing about whether
        // what the proxy forwarded is well-formed. `sanitize_sso_identity`
        // is that second question, and a rejected identity is treated
        // exactly like an absent one.
        let proxy_proved = match &proxy_secret {
            Some(presented) => {
                // `fold`, not `any`: `any` short-circuits on the first match,
                // so during a rotation window the response time would say
                // WHICH secret was presented. Every configured secret is
                // compared, every time.
                self.sso_secrets
                    .iter()
                    .fold(false, |acc, s| ct_eq(s.as_bytes(), presented.as_bytes()) | acc)
            }
            None => false,
        };
        let sso_identity: Option<String> = sso_identity_raw
            .filter(|_| proxy_proved)
            .as_deref()
            .and_then(sanitize_sso_identity);
        let sso_groups: Option<String> = sso_groups_raw.filter(|_| proxy_proved);
        // [A3] The OIDC login endpoints own their own responses: they answer
        // with redirects and `Set-Cookie`, which the JSON `route` contract
        // (status, content-type, body) has no slot for. Handled here, where
        // the socket and its headers are already in hand.
        #[cfg(feature = "oidc")]
        if self.oidc.is_some() && path.starts_with("/auth/") {
            let (status, extra, payload) =
                self.oidc_route(&method, &path, cookie_header.as_deref());
            drop(reader);
            let mut out = conn;
            write!(
                out,
                "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            out.write_all(payload.as_bytes())?;
            return out.flush();
        }
        #[cfg(feature = "oidc")]
        let oidc_principal = self.oidc_principal(cookie_header.as_deref());
        #[cfg(not(feature = "oidc"))]
        let oidc_principal: Option<String> = None;

        // [A1] Fail closed after repeated failures from one source.
        //
        // This REJECTS; it never delays. `serve` is a strictly serial accept
        // loop, so a per-request sleep would be a lever an unauthenticated
        // caller pulls to stall the console for everyone (the reasoning
        // recorded on `note_auth_failure`, which is why that counter only
        // ever counted). Rejection has the opposite shape: it is cheaper than
        // serving, costs the attacker their pipeline, and — placed HERE,
        // before `route` — touches no store, runs no constant-time scan, and
        // dispatches nothing.
        //
        // Only when auth is configured AND a credential was presented, the
        // same two conditions the counter itself uses: a console with no auth
        // has nothing to brute-force, and a browser's first credential-less
        // probe is not an attempt.
        //
        // AND NOT through a trusted proxy. Behind the documented deployment
        // (a TLS-terminating, authenticating proxy) every request shares the
        // proxy's source address, so a per-IP lockout would let one attacker's
        // ten bad guesses refuse *every* user behind it — a self-inflicted
        // outage in exactly the configuration we recommend. A request carrying
        // a verified proxy secret is proven to have come through that proxy,
        // and per-source throttling there is the proxy's job (the same
        // division of labour as TLS and the IdP handshake). Direct connections
        // — which is every connection on a console with no proxy — are
        // unaffected and still brake.
        let auth_configured =
            self.token.is_some() || self.credentials.is_some() || !self.sso_secrets.is_empty();
        let credential_presented = bearer.is_some() || proxy_secret_presented;
        if auth_configured && credential_presented && !proxy_proved {
            if let Ok(peer) = raw.peer_addr() {
                if self.auth_failure_count(peer.ip()) >= MAX_CONSECUTIVE_AUTH_FAILURES {
                    let payload = format!(
                        r#"{{"ok":false,"error":"too many failed authentications from this address - wait {} minutes, or restart the console to clear the counter"}}"#,
                        AUTH_FAILURE_IDLE_SECS / 60
                    );
                    drop(reader);
                    let mut out = conn;
                    write!(
                        out,
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        AUTH_FAILURE_IDLE_SECS,
                        payload.len()
                    )?;
                    out.write_all(payload.as_bytes())?;
                    return out.flush();
                }
            }
        }

        let (status, ctype, payload) = self.route_full(
            &method,
            &path,
            &body,
            bearer.as_deref(),
            sso_identity.as_deref(),
            sso_groups.as_deref(),
            oidc_principal.as_deref(),
        );

        // Auth-failure count + log (issue #126). Only when this server has
        // SOME auth mechanism configured, AND a credential was actually
        // PRESENTED (a Bearer/Basic value, or a proxy secret) — a plain
        // unauthenticated request (a token-less console's read-only-write
        // refusal, a browser's very first Basic-auth probe with no
        // Authorization header at all) attempted nothing and has no failure
        // to count. The token itself is never logged, not even a prefix.
        //
        // Deliberately no in-process DELAY here — see the comment on
        // `note_auth_failure` for why an artificial delay would itself be the
        // more dangerous bug on this server. The cheap rejection above is the
        // lockout; this is the counter that feeds it.
        if auth_configured && credential_presented {
            if let Ok(peer) = raw.peer_addr() {
                let ip = peer.ip();
                if status.starts_with("401") {
                    let consecutive = self.note_auth_failure(ip);
                    eprintln!("areev: console auth FAILED from {ip} ({consecutive} consecutive)");
                } else {
                    self.reset_auth_failures(ip);
                }
            }
        }

        // On a console-auth 401, challenge with Basic so browsers show the
        // native login prompt (any username; password = token).
        let auth_challenge = if self.auth_all && status.starts_with("401") {
            "WWW-Authenticate: Basic realm=\"Areev console\", charset=\"UTF-8\"\r\n"
        } else {
            ""
        };
        drop(reader);
        let mut out = conn;
        write!(
            out,
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{auth_challenge}Content-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )?;
        out.write_all(&payload)?;
        out.flush()
    }

    /// The runtime driver for the /api/run/* surface. The console never
    /// EXECUTES tools (list/inspect read; respond/cancel journal) — the
    /// executor is a refusing stub, present only to satisfy the driver
    /// shape.
    fn runner(&self, principal: &str) -> areev_run::Runner {
        struct NoExec;
        impl areev_run::HostToolExecutor for NoExec {
            fn execute(
                &self,
                tool_name: &str,
                _h: &str,
                _i: &Value,
                _k: &str,
            ) -> areev_run::ExecResult {
                areev_run::ExecResult::Err {
                    cause: areev_run::FailCause::ExecutorError,
                    detail: format!("the console does not execute tools ('{tool_name}')"),
                }
            }
        }
        areev_run::Runner {
            facade: std::sync::Arc::clone(&self.facade),
            clock: std::sync::Arc::new(areev_run::SystemClock),
            executor: std::sync::Arc::new(NoExec),
            llm: None,
            observer: None,
            ns: self
                .facade
                .default_namespace()
                .unwrap_or("shared")
                .to_string(),
            principal: principal.to_string(),
        }
    }

    /// The no-groups spelling, for the tests written before A2 added the
    /// groups header. Production always goes through
    /// [`route_with_groups`](Self::route_with_groups); this exists only so
    /// two dozen assertions that never had a groups header do not each grow
    /// a trailing `None`.
    #[cfg(test)]
    fn route(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        bearer: Option<&str>,
        sso_identity: Option<&str>,
    ) -> (&'static str, &'static str, Vec<u8>) {
        self.route_full(method, path, body, bearer, sso_identity, None, None)
    }

    /// [`route`](Self::route) with the A2 groups header but no OIDC session —
    /// the spelling the A2 tests use.
    #[cfg(test)]
    fn route_with_groups(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        bearer: Option<&str>,
        sso_identity: Option<&str>,
        sso_groups: Option<&str>,
    ) -> (&'static str, &'static str, Vec<u8>) {
        self.route_full(method, path, body, bearer, sso_identity, sso_groups, None)
    }

    /// Route one request.
    ///
    /// `sso_groups` is the raw, proxy-proven groups header (A2) and
    /// `oidc_principal` the identity behind a valid session cookie (A3) —
    /// both already authenticated by the caller, which is why neither
    /// carries its proof this far down.
    #[allow(clippy::too_many_arguments)]
    fn route_full(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        bearer: Option<&str>,
        sso_identity: Option<&str>,
        sso_groups: Option<&str>,
        oidc_principal: Option<&str>,
    ) -> (&'static str, &'static str, Vec<u8>) {
        // Auth: console-auth mode (`auth_all`) guards every request;
        // otherwise the token guards only mutating endpoints. The credential
        // arrives as a Bearer token or an HTTP Basic password (see
        // `handle_request`).
        //
        // A credential map changes WHO a request is, not WHETHER the shared
        // secret is required. When both are configured, a map token
        // authenticates as its principal, the shared secret authenticates as
        // the implied admin (`docs/cal-all-you-need-proposal.md` §5.2b), and
        // anything else is refused — an attached map must never silently
        // downgrade a `--token-env` console to an open `anonymous` one.
        let mut shared_secret_ok = false;
        if let Some(tok) = &self.token {
            let guarded = self.auth_all || method == "POST";
            shared_secret_ok = bearer.is_some_and(|b| ct_eq(b.as_bytes(), tok.as_bytes()));
            let known = shared_secret_ok
                || sso_identity.is_some()
                || oidc_principal.is_some()
                || match (&self.credentials, bearer) {
                    (Some(map), Some(b)) => map.resolve_for_memory(b, &self.db_label).is_ok(),
                    _ => false,
                };
            if guarded && !known {
                return ("401 Unauthorized", "application/json",
                        br#"{"ok":false,"error":"authentication required"}"#.to_vec());
            }
        } else if self.credentials.is_none()
            && sso_identity.is_none()
            && oidc_principal.is_none()
            && method == "POST"
        {
            // §5.7: token-less `areev ui` is read-only. The ONLY POST allowed is
            // a read-only CAL statement; every write (any loop mutation, an
            // ADD/SUPERSEDE/FORGET CAL batch, etc.) requires --token-env. This
            // closes the bypass where a local process could execute a
            // proposal's CAL directly and skip the review queue.
            let base = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
            let allowed = base == "/api/cal" && cal_body_is_read_only(body);
            if !allowed {
                return ("401 Unauthorized", "application/json",
                        br#"{"ok":false,"error":"read-only console: restart areev ui with --auth <map> (per-principal credentials, attributable writes and approvals) or --token-env VAR (one shared secret, writes only) to enable writes"}"#.to_vec());
            }
        }

        // Credential-map mode: resolve this request's principal and bind
        // the facade for the request's duration (the guard restores the
        // owner default on drop). Every verb check downstream — CAL,
        // loop scopes, the run gate below — now answers for THIS caller.
        // A request authenticated by the shared secret keeps the owner
        // session that secret has always carried.
        let _session = match (&self.credentials, shared_secret_ok) {
            (Some(map), false) => Some(RequestBinding::bind(&self.facade, map, bearer, &self.db_label)),
            _ => None,
        };
        // The request's resolved PRINCIPAL identity — Some only when a
        // per-principal credential authenticated it. The shared secret and
        // anonymous access deliberately resolve to None: approvals need an
        // identity, and "whoever holds the console token" is not one.
        let mut request_principal: Option<String> = match (&self.credentials, shared_secret_ok, bearer)
        {
            (Some(map), false, Some(b)) => map.resolve_for_memory(b, &self.db_label).ok().map(str::to_string),
            _ => None,
        };
        // SSO v0: a proxy-proven identity becomes the request principal when
        // no bearer credential resolved one (machines keep tokens; SSO is
        // for humans behind the proxy). Rights come from the file's grants.
        //
        // `principal_from_sso` records the PROVENANCE of the identity, not
        // just its value: everything downstream treats a principal as a
        // principal, but one control (HITL approval) must distinguish an
        // identity proven by a per-principal credential from one asserted by
        // a proxy holding a shared secret. See `sso_approvals`.
        let mut principal_from_sso = false;
        // [A2] `principal_from_group` is a strictly stronger refusal than
        // `principal_from_sso`: a role name cannot be an approver's identity
        // under ANY setting, because there is no person behind it.
        let mut principal_from_group = false;
        // [A3] An OIDC session outranks a proxy-asserted header: its identity
        // was proven by a signature this process verified against the
        // issuer's key set, not by a shared secret whose holder can assert
        // anyone. That is the whole reason native OIDC exists here, so it is
        // resolved FIRST and the header path never overwrites it.
        let mut principal_from_oidc = false;
        let _oidc_session = match (&request_principal, oidc_principal, shared_secret_ok) {
            (None, Some(principal), false) => {
                principal_from_oidc = true;
                let binding = RequestBinding::bind_identity(&self.facade, principal);
                request_principal = Some(principal.to_string());
                Some(binding)
            }
            _ => None,
        };
        let _sso_session = match (&request_principal, sso_identity, shared_secret_ok) {
            (None, Some(identity), false) => {
                match self.resolve_sso_principal(identity, sso_groups) {
                    Some((principal, from_group)) => {
                        principal_from_sso = true;
                        principal_from_group = from_group;
                        let binding = RequestBinding::bind_identity(&self.facade, &principal);
                        request_principal = Some(principal);
                        Some(binding)
                    }
                    None => None,
                }
            }
            _ => None,
        };

        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path, ""),
        };
        let q = |key: &str| -> Option<String> {
            query.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == key).then(|| urldecode(v))
            })
        };
        match (method, path) {
            ("GET", "/") => (
                "200 OK",
                "text/html; charset=utf-8",
                CONSOLE_HTML
                    .replace("{{DB}}", &html_escape(&self.db_label_display))
                    .into_bytes(),
            ),
            ("POST", "/api/cal") => {
                let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let queryt = req.get("query").and_then(|v| v.as_str()).unwrap_or("");
                match self.executor.execute(queryt, &*self.facade) {
                    Ok(res) => ok_json(json!({
                        "ok": true,
                        "result": res.result,
                        "warnings": res.warnings,
                        "statement": res.metadata.statement_type,
                        "elapsed_ms": res.metadata.execution_time_ms,
                    })),
                    Err(e) => {
                        // Structured error: code + span + hint let the console
                        // point at the offending token instead of just quoting.
                        let mut err = json!({
                            "ok": false,
                            "error": e.sanitize_for_client(),
                            "code": e.code(),
                        });
                        if let Some(sp) = e.span() {
                            err["span"] = json!({
                                "start": sp.start, "end": sp.end,
                                "line": sp.line, "col": sp.col,
                            });
                        }
                        if let Some(hint) = e.suggestion() {
                            err["suggestion"] = json!(hint);
                        }
                        ok_json(err)
                    }
                }
            }
            ("GET", "/api/stats") => {
                let stats = self.facade.with_store(|m| m.stats());
                match stats {
                    Ok(s) => ok_json(json!({
                        "db": self.db_label_display,
                        "grains": s.grains, "current": s.current,
                        "triples": s.triples, "terms": s.terms,
                        "ops": s.ops, "events_indexed": s.events_indexed,
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/log") => {
                let since: i64 = q("since").and_then(|v| v.parse().ok()).unwrap_or(0);
                let limit: usize = q("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
                match self.facade.with_store(|m| m.changes_since(since, limit)) {
                    Ok(ops) => {
                        let rows: Vec<Value> = ops
                            .iter()
                            .map(|o| {
                                json!({
                                    "op_seq": o.op_seq, "hlc": o.hlc,
                                    "op": match o.op { 1 => "add", 2 => "supersede", 3 => "forget", _ => "?" },
                                    "hash": o.hash.to_hex(),
                                })
                            })
                            .collect();
                        ok_json(json!(rows))
                    }
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            // Who am I: the principal this request is bound to. In
            // credential-map mode this reflects the per-request binding;
            // read-only and mode-agnostic.
            ("GET", "/api/whoami") => {
                let a = self.facade.authz();
                ok_json(json!({
                    "ok": true,
                    "principal": a.principal(),
                    "owner": a.is_owner(),
                    "mode": if self.credentials.is_some() { "credentials" }
                            else if self.token.is_some() { "token" }
                            else { "open" },
                    // Provenance, not just value: the console shows an
                    // approver whether their identity is one this instance
                    // will accept on `run.respond` BEFORE they try it.
                    "identity_source": if principal_from_oidc { "oidc" }
                                       else if principal_from_group { "sso-group" }
                                       else if principal_from_sso { "sso" }
                                       else if request_principal.is_some() { "credential" }
                                       else { "none" },
                    "may_approve": request_principal.is_some()
                        && !principal_from_group
                        && (!principal_from_sso || self.sso_approvals),
                }))
            }
            ("GET", "/api/config") => {
                // Read-only observability: the *effective* per-process
                // configuration. Nothing here is persisted in the .db —
                // the file holds data only; these values are supplied by
                // the host at open time.
                let cfg = self.executor.config();
                let (index_text, embedder_dim, declared_embed, mut warnings) =
                    self.facade.with_store(|m| {
                        (
                            m.index_text_enabled(),
                            m.embedder_dim(),
                            m.declared_embedding().map(|(mm, d)| (mm.to_string(), d)),
                            m.open_warnings().to_vec(),
                        )
                    });
                if let Some((m, d)) = &declared_embed {
                    if embedder_dim.is_none() {
                        warnings.push(format!(
                            "vector leg dormant: file expects {m}@{d}, no embedding backend installed"
                        ));
                    }
                }
                // Saved queries and templates the file carries that this build
                // cannot load. Same class of thing as an open warning — the
                // file and the host disagree — so it belongs in the same list.
                warnings.extend(self.facade.meta_warnings());
                // Anonymization observability (REQ-ANON-5: no policy is
                // loud): declared per-ns modes, the host floor, and the
                // audit counters. Modes only — never mappings or values.
                let (anon_declared, anon_floor, anon_audit) = self.facade.with_store(|m| {
                    (
                        m.anon_declared(),
                        m.anonymize_egress_floor(),
                        m.anon_audit_counts(),
                    )
                });
                let anonymization = json!({
                    "policies": anon_declared
                        .iter()
                        .map(|(ns, mode)| json!({"ns": ns, "mode": mode}))
                        .collect::<Vec<_>>(),
                    "floor": anon_floor,
                    "audit_counts": anon_audit
                        .iter()
                        .map(|(ns, cat, n)| json!({"ns": ns, "category": cat, "count": n}))
                        .collect::<Vec<_>>(),
                });
                ok_json(json!({
                    "ok": true,
                    "db": self.db_label_display,
                    "warnings": warnings,
                    "anonymization": anonymization,
                    "file": {
                        "text_index": index_text,
                        "embedding": declared_embed.map(|(m, d)| json!({"model": m, "dim": d})),
                    },
                    "session": {
                        "namespace": self.facade.session_namespace(),
                        "mounts": self.facade.mount_aliases(),
                        // Gates the console's "All namespaces" picker option —
                        // same rule /api/browse?ns=* and /api/namespaces enforce.
                        "is_owner": self.facade.authz().is_owner(),
                    },
                    "store": {
                        "index_text": index_text,
                        "embedder": embedder_dim.map(|d| json!({"dim": d})),
                    },
                    "recall": {
                        "fusion": "rrf",
                        "rrf_k0": areev_store::RRF_K0,
                        "overfetch_factor": AreevFacade::RECALL_OVERFETCH,
                        "legs": {
                            "structural": true,
                            "bm25": index_text,
                            "vector": embedder_dim.is_some(),
                        },
                    },
                    "executor": {
                        "max_limit": cfg.max_limit,
                        "default_limit": cfg.default_limit,
                        "tier1_writes": cfg.tier1_enabled,
                        "namespace_override": cfg.namespace_override,
                        "user_id_override": cfg.user_id_override,
                    },
                    "server": {
                        "auth_required": self.token.is_some(),
                        // true = every request is authenticated (console-auth);
                        // false with auth_required = writes only.
                        "auth_all": self.auth_all,
                    },
                    "persistence": "per-process (host-supplied at open) — not stored in the .db",
                }))
            }
            ("GET", "/api/browse") => {
                // Browse-without-queries: the tail of the op-log joined with
                // grain summaries, newest first. Supersession and tombstone
                // status are resolved within the returned window so the
                // console can dim/strike them (grains are never mutated —
                // this reads the index + immutable blobs only).
                let limit: i64 = q("limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(500)
                    .clamp(1, 2000);
                let built = self.facade.with_store(|m| -> Result<(i64, Vec<Value>), String> {
                    let total = m.stats().map_err(|e| e.to_string())?.ops as i64;
                    let after = (total - limit).max(0);
                    let ops = m.changes_since(after, limit as usize).map_err(|e| e.to_string())?;
                    let mut rows: Vec<Value> = Vec::with_capacity(ops.len());
                    let mut idx: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    for o in &ops {
                        let hex = o.hash.to_hex();
                        let op_name = match o.op { 1 => "add", 2 => "supersede", 3 => "forget", _ => "?" };
                        if o.op == 3 {
                            // tombstone: flag the earlier row if it is in the
                            // window, else emit a stub (blob is already gone).
                            match idx.get(&hex) {
                                Some(&i) => { rows[i]["forgotten"] = json!(true); }
                                None => rows.push(json!({
                                    "hash": hex, "op_seq": o.op_seq, "hlc": o.hlc,
                                    "op": op_name, "forgotten": true,
                                })),
                            }
                            continue;
                        }
                        // A SUPERSEDE logs two ops for the new grain (add +
                        // supersede) — merge them into one row, keeping the
                        // later op label.
                        if let Some(&i) = idx.get(&hex) {
                            rows[i]["op"] = json!(op_name);
                            rows[i]["op_seq"] = json!(o.op_seq);
                            rows[i]["hlc"] = json!(o.hlc);
                            continue;
                        }
                        let mut row = json!({
                            "hash": hex, "op_seq": o.op_seq, "hlc": o.hlc, "op": op_name,
                        });
                        match m.get(&o.hash) {
                            Ok(g) => {
                                row["type"] = json!(format!("{:?}", g.grain_type).to_lowercase());
                                row["fields"] = serde_json::to_value(&g.fields).unwrap_or(Value::Null);
                            }
                            // erased since (forget op outside the window)
                            Err(_) => { row["missing"] = json!(true); }
                        }
                        idx.insert(hex, rows.len());
                        rows.push(row);
                    }
                    // A supersede op's grain points at its predecessor via
                    // derived_from; mark the predecessor if it is in-window.
                    let marks: Vec<(String, String)> = rows.iter()
                        .filter(|r| r["op"] == "supersede")
                        .filter_map(|r| {
                            let old = r["fields"].get("derived_from")?.as_str()?;
                            Some((old.to_string(), r["hash"].as_str()?.to_string()))
                        })
                        .collect();
                    for (old, newer) in marks {
                        if let Some(&i) = idx.get(&old) {
                            rows[i]["superseded_by"] = json!(newer);
                        }
                    }
                    rows.reverse();
                    Ok((total, rows))
                });
                // Scope the browse to a namespace, the way CAL already scopes
                // recall. Without this the rail lists entities (`john`, `bob`)
                // that every query on this console then reports as missing,
                // because the two disagree about what is in view.
                //
                // `?ns=<name>` lets the console browse a namespace other than
                // the one it was launched with, and `?ns=*` lifts the filter
                // entirely — both read-gated against the session's own
                // `AuthzSet` exactly as a `RECALL ... WHERE namespace = ...`
                // would be (`changes_since` itself reads the whole op-log
                // unscoped; this filter is the only enforcement point, so it
                // has to check what `AreevFacade::recall` checks, not less).
                // Omitting `?ns` keeps the original session-default behavior
                // byte-for-byte. Rows we cannot attribute — tombstone stubs,
                // grains erased since — are kept regardless: dropping them
                // would hide an erasure.
                let ns_param = q("ns");
                let built = built.and_then(|(total, rows)| {
                    let authz = self.facade.authz();
                    let filter_ns = match ns_param.as_deref() {
                        Some("*") => {
                            if !authz.is_owner() {
                                return Err("not authorized to browse all namespaces".to_string());
                            }
                            None
                        }
                        Some(ns) => {
                            authz
                                .check(areev_core::authz::Verb::Read, ns)
                                .map_err(|e| e.to_string())?;
                            Some(ns.to_string())
                        }
                        None => self.facade.session_namespace().map(|s| s.to_string()),
                    };
                    let Some(ns) = filter_ns else {
                        return Ok((total, rows));
                    };
                    let kept = rows
                        .into_iter()
                        .filter(|r| {
                            r["fields"]
                                .get("namespace")
                                .and_then(Value::as_str)
                                .is_none_or(|got| got == ns)
                        })
                        .collect();
                    Ok((total, kept))
                });
                match built {
                    Ok((total, rows)) => ok_json(json!({"ok": true, "total_ops": total, "grains": rows})),
                    Err(e) => ok_json(json!({"ok": false, "error": e})),
                }
            }
            ("GET", "/api/namespaces") => {
                // Every namespace present in the file, for the console's
                // namespace picker. A restricted principal only ever sees the
                // ones its own grants cover — read access to the *names* of
                // namespaces it cannot read from would itself be a disclosure.
                let result = self.facade.with_store(|m| m.namespaces());
                match result {
                    Ok(list) => {
                        let authz = self.facade.authz();
                        let owner = authz.is_owner();
                        let arr: Vec<Value> = list
                            .into_iter()
                            .filter(|(ns, _)| {
                                owner || authz.check(areev_core::authz::Verb::Read, ns).is_ok()
                            })
                            .map(|(ns, n)| json!({"namespace": ns, "count": n}))
                            .collect();
                        ok_json(json!({"ok": true, "namespaces": arr}))
                    }
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/grain") => {
                let hash = q("hash").unwrap_or_default();
                match areev_core::error::Hash::from_hex(&hash)
                    .and_then(|h| self.facade.get(&h))
                {
                    Ok(g) => ok_json(json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "fields": g.fields,
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/anon/preview") => {
                // "As the model sees it" (proposal §8.3): the grain rendered
                // the way an egress boundary would show it, policy or not.
                let hash = q("hash").unwrap_or_default();
                match areev_core::error::Hash::from_hex(&hash)
                    .and_then(|h| self.facade.with_store(|m| m.anon_preview(&h)))
                {
                    Ok(g) => ok_json(json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "fields": g.fields,
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/verify") => match self.facade.with_store(|m| m.verify()) {
                Ok(r) => ok_json(json!({
                    "integrity": r.integrity, "grains": r.grains,
                    "hash_mismatches": r.hash_mismatches, "undecodable": r.undecodable,
                })),
                Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
            },
            // ── Areev Loop API (§5.4) — GETs are reads (token-less OK); the POST
            //    mutations are guarded above (token-less → 401). ────────────
            // ── /api/run/*: the governed runtime's console surface ──
            ("GET", "/api/run/list") => {
                // Paged and namespace-scoped on the server (#165): the
                // console used to filter a fixed newest-50 page client-side,
                // so a quiet tenant's runs — open approvals included — were
                // simply absent behind a busier tenant's. `ns` takes an exact
                // name or an `org.*` prefix; `limit` is clamped so one
                // request cannot ask for the whole history; the response
                // says the total and whether it truncated.
                let runner = self.runner("console:read");
                let ns = q("ns").filter(|s| !s.is_empty() && s.as_str() != "*");
                let limit = q("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(50)
                    .clamp(1, 500);
                let offset = q("offset").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                match runner.list_runs(ns.as_deref(), offset, limit) {
                    Ok(page) => {
                        let rows: Vec<Value> = page
                            .runs
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "run_id": r.run_id,
                                    "ns": r.ns,
                                    "outcome": r.outcome,
                                })
                            })
                            .collect();
                        ok_json(serde_json::json!({
                            "ok": true,
                            "runs": rows,
                            "total": page.total,
                            "truncated": page.truncated,
                            "offset": page.offset,
                            "limit": page.limit,
                            "unattributed": page.unattributed,
                        }))
                    }
                    Err(e) => run_api_err(&e.to_string()),
                }
            }
            ("GET", "/api/run/inspect") => {
                let Some(run_id) = q("run_id") else {
                    return run_api_err("run_id required");
                };
                // Same Runner::inspect the CLI and bindings call (issue
                // #34) — this used to hand-roll a divergent, smaller subset
                // (no pinned/budgets/fork_of/superstep/phase).
                match self.runner("console:read").inspect(&run_id) {
                    Ok(report) => {
                        let mut v = serde_json::to_value(&report).unwrap_or(Value::Null);
                        if let Some(o) = v.as_object_mut() {
                            o.insert("ok".into(), Value::Bool(true));
                        }
                        ok_json(v)
                    }
                    Err(e) => run_api_err(&e.to_string()),
                }
            }
            ("POST", "/api/run/respond") => {
                // [R3] SHARED-TOKEN APPROVALS ARE REFUSED: an approval whose
                // "identity" is a console-wide secret voids the
                // approver-identity edge of the provenance chain. Only a
                // per-principal credential (areev ui --auth) may respond.
                let Some(responder) = request_principal.clone() else {
                    return ("403 Forbidden", "application/json",
                        br#"{"ok":false,"error":"run.respond requires a per-principal credential (areev ui --auth <map>); shared-token or anonymous approvals are refused - the approver's identity IS the audit record"}"#.to_vec());
                };
                // [A0] PROXY-ASSERTED APPROVALS ARE REFUSED BY DEFAULT: an
                // SSO identity's only proof is a shared, static, fleet-wide
                // proxy secret — whoever holds it can assert any identity,
                // including this approver's. That is a weaker claim than the
                // per-principal credential [R3] demands, and the resulting
                // audit grain would be indistinguishable from a real
                // approval. Opt in with `areev ui --sso-approvals allow`.
                if principal_from_sso && !self.sso_approvals {
                    return ("403 Forbidden", "application/json",
                        br#"{"ok":false,"error":"run.respond refuses proxy-asserted SSO identities: the identity header is trusted only via a shared proxy secret, which whoever holds it can use to assert any approver. Approve with a per-principal credential (areev ui --auth <map>), or accept the trade-off explicitly with --sso-approvals allow"}"#.to_vec());
                }
                // [A2] A group-derived principal is a ROLE. It is refused
                // even under `--sso-approvals allow`, and deliberately has no
                // flag of its own: the audit record would read "role:eng
                // approved", which names no one who can be asked why.
                if principal_from_group {
                    return ("403 Forbidden", "application/json",
                        br#"{"ok":false,"error":"run.respond refuses group-derived principals: a role is not an approver, and an audit record naming one identifies nobody. Grant this person a principal of their own, or give them a per-principal credential"}"#.to_vec());
                }
                let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let (Some(run_id), Some(ask)) = (
                    req.get("run_id").and_then(Value::as_str),
                    req.get("tool_call_id").and_then(Value::as_str),
                ) else {
                    return run_api_err("run_id and tool_call_id required");
                };
                let result = req.get("result").cloned().unwrap_or(Value::Null);
                let is_error = req.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let runner = self.runner(&responder);
                match runner.respond(run_id, ask, result, is_error, &responder) {
                    Ok(()) => ok_json(serde_json::json!({
                        "ok": true, "responded": ask, "responder": responder,
                        "note": "resume the run to spend the compute",
                    })),
                    Err(e) => run_api_err(&e.to_string()),
                }
            }
            ("POST", "/api/run/cancel") => {
                // The kill switch keeps its LOW bar: any authenticated POST
                // may brake a run (the identity label still records who).
                let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let Some(run_id) = req.get("run_id").and_then(Value::as_str) else {
                    return run_api_err("run_id required");
                };
                let because = req.get("because").and_then(Value::as_str).unwrap_or("canceled via console");
                let who = request_principal.clone().unwrap_or_else(|| "console:shared".into());
                let runner = self.runner(&who);
                match runner.cancel(run_id, &who, because) {
                    Ok(()) => ok_json(serde_json::json!({"ok": true, "canceled": run_id})),
                    Err(e) => run_api_err(&e.to_string()),
                }
            }
            ("GET", "/api/loop/recommendations") => {
                let status = q("status").and_then(|s| status_from_str(&s));
                let sub = BorrowedSubstrate::new(&self.facade);
                match self.engine().recommendations(&sub, status) {
                    Ok(recs) => ok_json(json!({
                        "ok": true,
                        "recommendations": recs.iter().map(rec_json).collect::<Vec<_>>(),
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/loop/health") => {
                let sub = BorrowedSubstrate::new(&self.facade);
                match self.engine().health(&sub, now_ms()) {
                    Ok(h) => ok_json(json!({"ok": true, "health": h})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/loop/outcomes") => {
                let sub = BorrowedSubstrate::new(&self.facade);
                match self.engine().outcomes(&sub) {
                    Ok(o) => ok_json(json!({"ok": true, "outcomes": o})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/loop/analyzers") => {
                // Effective settings (manifest merged with the file-config), so
                // the Setup view renders accurate on/off state and floors.
                let sub = BorrowedSubstrate::new(&self.facade);
                match self.engine().analyzer_settings(&sub) {
                    Ok(list) => ok_json(json!({"ok": true, "analyzers": list})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/loop/telemetry") => {
                // Recall-telemetry rollups for the Sessions view. A read — open
                // in read-only mode like the other loop GETs.
                let mode = self.facade.with_store(|m| m.telemetry_mode());
                if mode == areev_store::TelemetryMode::Off {
                    ok_json(json!({"ok": true, "enabled": false}))
                } else {
                    let access = self.facade.with_store(|m| m.telemetry_access_stats(None));
                    let queries = self.facade.with_store(|m| m.telemetry_query_stats(None));
                    let budget = self.facade.with_store(|m| m.telemetry_budget_stats());
                    match (access, queries, budget) {
                        (Ok(mut a), Ok(mut q), Ok(b)) => {
                            // Most-recalled first; recurring-gap questions first.
                            a.sort_by_key(|x| std::cmp::Reverse(x.recall_count));
                            a.truncate(200);
                            q.sort_by_key(|x| std::cmp::Reverse(x.run_count));
                            q.truncate(200);
                            ok_json(json!({
                                "ok": true,
                                "enabled": true,
                                "mode": mode.as_str(),
                                "access": a.iter().map(|x| json!({
                                    "hash": x.hash, "recall_count": x.recall_count, "last_ms": x.last_ms,
                                })).collect::<Vec<_>>(),
                                "queries": q.iter().map(|x| json!({
                                    "sample": x.sample, "run_count": x.run_count,
                                    "empty_count": x.empty_count,
                                })).collect::<Vec<_>>(),
                                "budget": {
                                    "sample_count": b.sample_count,
                                    "overflow_count": b.overflow_count,
                                },
                            }))
                        }
                        _ => ok_json(json!({"ok": false, "error": "telemetry read failed"})),
                    }
                }
            }
            ("POST", "/api/loop/run") => {
                let authz = self.facade.authz();
                if !authz.is_owner()
                    && !authz.allows(areev_core::authz::Verb::LoopRun, areev_loop::LOOP_NS)
                {
                    return ("403 Forbidden", "application/json",
                            br#"{"ok":false,"error":"AUT-E001: this session lacks loop.run"}"#.to_vec());
                }
                let mut sub = BorrowedSubstrate::new(&self.facade);
                // The trigger is recorded as co-creator on LLM/external
                // findings, so the same principal cannot later approve them.
                // In credential-map mode the actor IS the bound principal —
                // a request cannot claim an identity.
                let actor = if self.credentials.is_some() {
                    authz.principal().to_string()
                } else {
                    serde_json::from_slice::<Value>(body)
                        .ok()
                        .and_then(|v| v.get("actor").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| "user:console".to_string())
                };
                let opts = RunOptions { triggering_actor: Some(actor), ..Default::default() };
                match self.engine().run(&mut sub, &opts, now_ms()) {
                    Ok(res) => ok_json(json!({"ok": true, "run": res})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("POST", "/api/loop/review") => self.loop_review(body),
            ("POST", "/api/loop/apply") => self.loop_apply(body),
            ("POST", "/api/loop/rollback") => self.loop_rollback(body),
            ("POST", "/api/loop/config") => self.loop_config(body),
            ("POST", "/api/anon/config") => self.anon_config(body),
            _ => (
                "404 Not Found",
                "application/json",
                br#"{"ok":false,"error":"not found"}"#.to_vec(),
            ),
        }
    }

    /// The console session's scopes derive from its grants (an owner
    /// session — today's only mode — holds all of them); actor
    /// `user:console` unless overridden, observer derived from the actor
    /// label, never from request text.
    fn loop_review(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let hash = req.get("hash").and_then(Value::as_str).unwrap_or("");
        let because = req.get("because").and_then(Value::as_str).unwrap_or("");
        let bound = self.facade.authz();
        let actor = if self.credentials.is_some() {
            bound.principal().to_string()
        } else {
            req.get("actor").and_then(Value::as_str).unwrap_or("user:console").to_string()
        };
        let actor = actor.as_str();
        let decision = if req.get("decision").and_then(Value::as_str) == Some("reject") {
            Decision::Reject
        } else {
            Decision::Approve
        };
        let mut sub = BorrowedSubstrate::new(&self.facade);
        let scopes = areev_loop_adapter::scopes_for(&self.facade.authz());
        let observer = areev_loop_adapter::observer_for_principal(actor);
        match self.engine().review(
            &mut sub, hash, decision, actor, observer, &scopes, because, now_ms(),
        ) {
            Ok(()) => ok_json(json!({"ok": true})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }

    fn loop_apply(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let hash = req.get("hash").and_then(Value::as_str).unwrap_or("");
        let because = req.get("because").and_then(Value::as_str).unwrap_or("");
        let bound = self.facade.authz();
        let actor = if self.credentials.is_some() {
            bound.principal().to_string()
        } else {
            req.get("actor").and_then(Value::as_str).unwrap_or("user:console").to_string()
        };
        let actor = actor.as_str();
        let allow_destructive = req.get("allow_destructive").and_then(Value::as_bool).unwrap_or(false);
        let mut sub = BorrowedSubstrate::new(&self.facade);
        let scopes = areev_loop_adapter::scopes_for(&self.facade.authz());
        let observer = areev_loop_adapter::observer_for_principal(actor);
        let engine = self.engine();
        // A code or adapter revision applies only through its recorded gating
        // edge: `gating_run` names an eval run whose journaled `mg:eval_run`
        // summary becomes the evidence — the stats never come from the client.
        let gating = match req.get("gating_run").and_then(Value::as_str) {
            Some(run_id) => match engine.gating_evidence(&sub, hash, run_id) {
                Ok(g) => Some(g),
                Err(e) => {
                    return ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()}))
                }
            },
            None => None,
        };
        let applied = match &gating {
            Some(g) => engine.apply_gated(
                &mut sub, hash, actor, observer, &scopes, because, allow_destructive, g, now_ms(),
            ),
            None => engine.apply(
                &mut sub, hash, actor, observer, &scopes, because, allow_destructive, now_ms(),
            ),
        };
        match applied {
            Ok(applied) => ok_json(json!({"ok": true, "rollbackable": applied.rollbackable})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }

    fn loop_rollback(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let hash = req.get("hash").and_then(Value::as_str).unwrap_or("");
        let because = req.get("because").and_then(Value::as_str).unwrap_or("");
        let bound = self.facade.authz();
        let actor = if self.credentials.is_some() {
            bound.principal().to_string()
        } else {
            req.get("actor").and_then(Value::as_str).unwrap_or("user:console").to_string()
        };
        let actor = actor.as_str();
        let mut sub = BorrowedSubstrate::new(&self.facade);
        let scopes = areev_loop_adapter::scopes_for(&self.facade.authz());
        let observer = areev_loop_adapter::observer_for_principal(actor);
        match self.engine().rollback(
            &mut sub, hash, actor, observer, &scopes, because, now_ms(),
        ) {
            Ok(()) => ok_json(json!({"ok": true})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }

    /// Edit one analyzer's file-config from the Setup view. The body is
    /// `{analyzer_id, enabled?, severity_floor?, clear_floor?, params?,
    /// namespaces?}`; absent fields are left unchanged. The console holds all
    /// scopes (local root of trust), so `Admin` is satisfied; an unknown
    /// analyzer or bad param is a structured `ok:false` (not a 500).
    fn loop_config(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let id = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("analyzer_id").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        // The update reads the same body; the extra `analyzer_id` key is ignored.
        let update: areev_loop::AnalyzerConfigUpdate = serde_json::from_slice(body).unwrap_or_default();
        let mut sub = BorrowedSubstrate::new(&self.facade);
        match self.engine().set_analyzer_config(&mut sub, &id, update, &areev_loop_adapter::scopes_for(&self.facade.authz())) {
            Ok(cfg) => ok_json(json!({"ok": true, "config": cfg})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }
}

impl UiServer {
    /// Declare or clear a per-namespace anonymization policy from the
    /// console (Connect → Settings). Rides the same write gate as every
    /// console POST (auth + Origin + body cap upstream); the policy itself
    /// validates fail-closed in the store.
    fn anon_config(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let v: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return ok_json(json!({"ok": false, "error": e.to_string()})),
        };
        let Some(ns) = v.get("ns").and_then(Value::as_str).filter(|n| !n.trim().is_empty())
        else {
            return ok_json(json!({"ok": false, "error": "ns is required"}));
        };
        let result = if v.get("clear").and_then(Value::as_bool) == Some(true) {
            self.facade.with_store(|m| m.clear_anon_policy(ns))
        } else {
            match v.get("policy") {
                Some(policy) => {
                    let policy_json = policy.to_string();
                    self.facade.with_store(|m| m.set_anon_policy(ns, &policy_json))
                }
                None => {
                    return ok_json(json!({"ok": false, "error": "policy (object) or clear: true is required"}));
                }
            }
        };
        match result {
            Ok(()) => {
                let declared = self.facade.with_store(|m| m.anon_declared());
                ok_json(json!({"ok": true, "policies": declared
                    .iter()
                    .map(|(ns, mode)| json!({"ns": ns, "mode": mode}))
                    .collect::<Vec<_>>()}))
            }
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
        }
    }
}

/// True when a `host[:port]` (or `[ipv6]:port`) authority names a loopback
/// host. Shared by the Origin drive-by check and the Host-header
/// (DNS-rebinding) check.
fn host_is_local(host_port: &str) -> bool {
    let host = if let Some(h) = host_port.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// True when an Origin header names a loopback host (any port, http/https).
fn origin_is_local(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    let host_port = rest.split('/').next().unwrap_or("");
    host_is_local(host_port)
}

/// Normalize an origin (an `Origin` header value, or an operator-configured
/// `--allow-origin` entry) for EXACT comparison: lowercase, with a single
/// trailing `/` dropped. Nothing else — this only makes two spellings of the
/// same origin compare equal; it does not validate shape (the CLI's
/// `--allow-origin` parser does that at startup).
fn normalize_origin(origin: &str) -> String {
    let trimmed = origin.trim();
    trimmed.strip_suffix('/').unwrap_or(trimmed).to_ascii_lowercase()
}

/// Redact a DISPLAY copy of a connection-string / file-path DB label
/// (issue #124): if `label` has a `scheme://[user[:pass]@]host...` shape and
/// the userinfo carries a password, replace the password with `***`.
/// Anything else — a plain file path (no `://`), a DSN with no userinfo, or
/// userinfo with no password — is returned byte-for-byte unchanged.
///
/// **DISPLAY ONLY.** The raw label is still what
/// `CredentialMap::resolve_for_memory` (`areev-core/src/authz.rs`) matches a
/// token's `memories` scope against via an exact string compare — never swap
/// in this redacted form at those call sites, or every `--auth FILE`
/// deployment that scopes a token to a Postgres memory silently breaks.
///
/// Hand-rolled (no URL crate, per the workspace's dependency-light policy):
/// only the AUTHORITY segment — between `://` and the first `/`, `?`, or `#`
/// that follows — is considered, and within it the LAST `@` is taken as the
/// userinfo delimiter. RFC 3986 allows a percent-encoded `@` in a password
/// and real passwords contain literal `@` too, so an earlier `@` is presumed
/// to be *inside* the password rather than a second delimiter; and a `:` or
/// `@` appearing later — in a query string, say — is outside the authority
/// entirely and is never mistaken for userinfo.
pub fn redact_dsn(label: &str) -> String {
    let Some(scheme_end) = label.find("://") else {
        return label.to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &label[authority_start..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_len];
    let Some(at) = authority.rfind('@') else {
        return label.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return label.to_string();
    };
    let user = &userinfo[..colon];
    let mut out = String::with_capacity(label.len());
    out.push_str(&label[..authority_start]);
    out.push_str(user);
    out.push_str(":***@");
    out.push_str(&authority[at + 1..]);
    out.push_str(&rest[authority_len..]);
    out
}

fn ok_json(v: Value) -> (&'static str, &'static str, Vec<u8>) {
    ("200 OK", "application/json", v.to_string().into_bytes())
}

fn run_api_err(msg: &str) -> (&'static str, &'static str, Vec<u8>) {
    (
        "400 Bad Request",
        "application/json",
        serde_json::json!({"ok": false, "error": msg}).to_string().into_bytes(),
    )
}

/// True when a `POST /api/cal` body is a read-only statement. Classification
/// is delegated to `areev-cal`'s exhaustive [`areev_cal::classify`] module —
/// the single source of truth — so grammar growth cannot desynchronize this
/// gate (a keyword sniff lived here before and would have silently
/// misclassified any new statement). Fail closed: unparseable text is not
/// read-only.
fn cal_body_is_read_only(body: &[u8]) -> bool {
    let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let q = req.get("query").and_then(Value::as_str).unwrap_or("");
    areev_cal::classify::query_is_read_only(q)
}

fn status_from_str(s: &str) -> Option<RecStatus> {
    match s {
        "pending" => Some(RecStatus::Pending),
        "approved" => Some(RecStatus::Approved),
        "rejected" => Some(RecStatus::Rejected),
        "applied" => Some(RecStatus::Applied),
        "rolled_back" => Some(RecStatus::RolledBack),
        "expired" => Some(RecStatus::Expired),
        _ => None, // includes "all"
    }
}

fn rec_json(r: &areev_loop::Recommendation) -> Value {
    json!({
        "hash": r.hash,
        "status": r.status.as_str(),
        "severity": r.severity.as_str(),
        "analyzer": r.analyzer,
        "summary": r.summary.render(),
        "target_ref": r.target_ref,
        "destructive": r.destructive,
        "rollbackable": r.rollbackable,
        "evidence": r.evidence,
        // The Rule E1 pin, when present — the console reads it to know this
        // apply needs a recorded gating run (and which evalset to run).
        "evalset_hash": r.evalset_hash,
    })
}

fn urldecode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() + 1 && i + 2 < b.len() + 1 => {
                if i + 2 < b.len() + 1 && i + 2 < b.len() {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Constant-time byte comparison — avoids leaking the bearer token through
/// response timing. A length mismatch fails fast (token length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Decode standard base64 (RFC 4648), enough for HTTP Basic credentials.
/// The one hand-rolled decoder lives in `areev_core::b64` (the trigger
/// evaluator shares it for connector blobs, #93).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    areev_core::b64::decode(s)
}

/// Extract the password from an HTTP Basic credential (`base64("user:pass")`).
/// The console ignores the username and treats the password as the token.
/// Returns `None` if the value is not valid base64/UTF-8 or has no `:`.
fn basic_auth_password(b64: &str) -> Option<String> {
    let text = String::from_utf8(base64_decode(b64)?).ok()?;
    text.split_once(':').map(|(_user, pass)| pass.to_string())
}

#[cfg(test)]
mod loop_route_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    fn server(auth: Option<&str>) -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        // Two identical facts → a duplicate-consolidation recommendation.
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller")).unwrap();
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller")).unwrap();
        std::mem::forget(dir); // keep the file alive for the server's lifetime
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let s = UiServer::new(facade, "test".into());
        match auth {
            Some(t) => s.with_auth(t.to_string()),
            None => s,
        }
    }

    fn text(r: &(&str, &str, Vec<u8>)) -> String {
        String::from_utf8_lossy(&r.2).to_string()
    }

    #[test]
    fn token_less_console_is_read_only() {
        let s = server(None);
        // A loop mutation (run) is a write → 401.
        assert!(s.route("POST", "/api/loop/run", b"{}", None, None).0.starts_with("401"));
        // A write CAL → 401.
        let w = s.route("POST", "/api/cal", br#"{"query":"ADD fact SET subject=\"x\""}"#, None, None);
        assert!(w.0.starts_with("401"), "write CAL must be 401: {}", text(&w));
        // A read CAL → 200.
        let r = s.route("POST", "/api/cal", br#"{"query":"RECALL facts WHERE subject = \"acme\""}"#, None, None);
        assert!(r.0.starts_with("200"), "read CAL allowed: {}", text(&r));
        // Areev Loop reads stay open token-less.
        assert!(s.route("GET", "/api/loop/recommendations", b"", None, None).0.starts_with("200"));
    }

    #[test]
    fn telemetry_endpoint_is_an_open_read() {
        let s = server(None);
        let r = s.route("GET", "/api/loop/telemetry", b"", None, None);
        assert!(r.0.starts_with("200"), "telemetry read open: {}", text(&r));
        let body = text(&r);
        assert!(body.contains("\"ok\":true"), "{body}");
        // The test store opens bare (telemetry off) → enabled:false.
        assert!(body.contains("\"enabled\":false"), "{body}");
    }

    #[test]
    fn authenticated_run_review_apply_roundtrip() {
        let s = server(Some("tok"));
        let run = s.route("POST", "/api/loop/run", b"{}", Some("tok"), None);
        assert!(run.0.starts_with("200"), "run: {}", text(&run));
        assert!(text(&run).contains("\"ran\""));

        let list = s.route("GET", "/api/loop/recommendations?status=pending", b"", Some("tok"), None);
        let v: serde_json::Value = serde_json::from_slice(&list.2).unwrap();
        let recs = v["recommendations"].as_array().unwrap();
        assert!(!recs.is_empty(), "at least one recommendation");
        let hash = recs[0]["hash"].as_str().unwrap().to_string();

        let rev = s.route(
            "POST",
            "/api/loop/review",
            format!(r#"{{"hash":"{hash}","decision":"approve","because":"ok"}}"#).as_bytes(),
            Some("tok"),
            None,
        );
        assert!(text(&rev).contains("\"ok\":true"), "review: {}", text(&rev));

        let ap = s.route(
            "POST",
            "/api/loop/apply",
            format!(r#"{{"hash":"{hash}","because":"go"}}"#).as_bytes(),
            Some("tok"),
            None,
        );
        assert!(text(&ap).contains("\"ok\":true"), "apply: {}", text(&ap));
    }

    #[test]
    fn config_edit_toggles_analyzer_via_console() {
        let s = server(Some("tok"));
        // Token-less write → 401 (guarded like every POST).
        assert!(s.route("POST", "/api/loop/config", b"{}", None, None).0.starts_with("401"));

        // Read the analyzers; pick one that is on by default.
        let list = s.route("GET", "/api/loop/analyzers", b"", Some("tok"), None);
        let v: serde_json::Value = serde_json::from_slice(&list.2).unwrap();
        let id = v["analyzers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["enabled"] == true)
            .expect("some analyzer is on")["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Disable it via the console endpoint.
        let post = s.route(
            "POST",
            "/api/loop/config",
            format!(r#"{{"analyzer_id":"{id}","enabled":false}}"#).as_bytes(),
            Some("tok"),
            None,
        );
        assert!(text(&post).contains("\"ok\":true"), "config: {}", text(&post));

        // It reads back disabled.
        let list2 = s.route("GET", "/api/loop/analyzers", b"", Some("tok"), None);
        let v2: serde_json::Value = serde_json::from_slice(&list2.2).unwrap();
        let now = v2["analyzers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id.as_str())
            .unwrap();
        assert_eq!(now["enabled"], false, "toggled off and persisted");
    }

    #[test]
    fn self_approval_surfaces_lop_code() {
        let s = server(Some("tok"));
        s.route("POST", "/api/loop/run", b"{}", Some("tok"), None);
        let list = s.route("GET", "/api/loop/recommendations?status=pending", b"", Some("tok"), None);
        let v: serde_json::Value = serde_json::from_slice(&list.2).unwrap();
        let rec = &v["recommendations"].as_array().unwrap()[0];
        let hash = rec["hash"].as_str().unwrap();
        let analyzer = rec["analyzer"].as_str().unwrap();
        let creator = format!("engine:{analyzer}");
        // The engine actor approving its own proposal is blocked.
        let rev = s.route(
            "POST",
            "/api/loop/review",
            format!(r#"{{"hash":"{hash}","decision":"approve","because":"self","actor":"{creator}"}}"#).as_bytes(),
            Some("tok"),
            None,
        );
        assert!(text(&rev).contains("LOP-E021"), "self-approval blocked: {}", text(&rev));
    }
}

#[cfg(test)]
mod security_tests {
    use super::{base64_decode, basic_auth_password, ct_eq, normalize_origin, redact_dsn};

    #[test]
    fn ct_eq_matches_only_equal() {
        assert!(ct_eq(b"secret-token", b"secret-token"));
        assert!(!ct_eq(b"secret-token", b"secret-toker"));
        assert!(!ct_eq(b"secret", b"secret-token")); // length mismatch
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn base64_decode_roundtrips_known_vectors() {
        // RFC 4648 test vectors (with and without padding).
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        // padding is optional for us
        assert_eq!(base64_decode("Zg").unwrap(), b"f");
        // invalid characters are rejected
        assert!(base64_decode("****").is_none());
    }

    #[test]
    fn basic_auth_extracts_password_ignoring_username() {
        // base64("areev:s3cret") and base64(":s3cret") both yield the token.
        assert_eq!(basic_auth_password("ZGVqYTpzM2NyZXQ=").as_deref(), Some("s3cret"));
        assert_eq!(basic_auth_password("OnMzY3JldA==").as_deref(), Some("s3cret"));
        // a password may itself contain ':' — only the first ':' splits.
        // base64("u:a:b") -> "a:b"
        assert_eq!(basic_auth_password("dTphOmI=").as_deref(), Some("a:b"));
        // no ':' at all → not a valid Basic credential
        assert_eq!(basic_auth_password("bm9jb2xvbg=="), None); // "nocolon"
        assert_eq!(basic_auth_password("****"), None); // not base64
    }

    // ── redact_dsn (issue #124) ─────────────────────────────────────────

    #[test]
    fn redact_dsn_hides_a_password() {
        assert_eq!(
            redact_dsn("postgresql://rounic:SUPERSECRET@pg-x:5432/rounic?sslmode=verify-full&schema=desk_invoice"),
            "postgresql://rounic:***@pg-x:5432/rounic?sslmode=verify-full&schema=desk_invoice",
        );
    }

    #[test]
    fn redact_dsn_leaves_no_password_alone() {
        assert_eq!(redact_dsn("postgres://user@host:5432/db"), "postgres://user@host:5432/db");
    }

    #[test]
    fn redact_dsn_leaves_no_userinfo_alone() {
        assert_eq!(redact_dsn("postgres://host:5432/db"), "postgres://host:5432/db");
    }

    #[test]
    fn redact_dsn_leaves_a_plain_file_path_alone() {
        assert_eq!(redact_dsn("demo.db"), "demo.db");
        assert_eq!(redact_dsn("/var/lib/areev/demo.db"), "/var/lib/areev/demo.db");
    }

    #[test]
    fn redact_dsn_hides_a_percent_encoded_password() {
        // %40 is an encoded '@' inside the password — the LITERAL '@' that
        // ends the authority is the one after it.
        assert_eq!(
            redact_dsn("postgres://user:pa%40ss@host:5432/db"),
            "postgres://user:***@host:5432/db",
        );
    }

    #[test]
    fn redact_dsn_hides_a_password_with_a_literal_at_sign() {
        // The LAST '@' in the authority is the userinfo/host boundary, so a
        // literal '@' inside the password is presumed to be part of it.
        assert_eq!(
            redact_dsn("postgres://user:p@ss@host:5432/db"),
            "postgres://user:***@host:5432/db",
        );
    }

    #[test]
    fn redact_dsn_ignores_colon_and_at_in_the_query_string() {
        assert_eq!(
            redact_dsn("postgres://user:pass@host:5432/db?options=-c%20foo:bar@baz"),
            "postgres://user:***@host:5432/db?options=-c%20foo:bar@baz",
        );
    }

    #[test]
    fn redact_dsn_handles_empty_string() {
        assert_eq!(redact_dsn(""), "");
    }

    // ── normalize_origin (issue #125) ───────────────────────────────────

    #[test]
    fn normalize_origin_lowercases_and_drops_one_trailing_slash() {
        assert_eq!(normalize_origin("https://Console.Example.com"), "https://console.example.com");
        assert_eq!(normalize_origin("https://console.example.com/"), "https://console.example.com");
        assert_eq!(
            normalize_origin("HTTPS://Console.Example.com/"),
            "https://console.example.com",
        );
    }
}

#[cfg(test)]
mod credential_route_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    /// The tuning seam over HTTP: an adapter recommendation applies only
    /// with `gating_run` in the POST body — evidence loaded from the
    /// journaled eval summary, never from the client — and the rec row
    /// carries `evalset_hash` so the console knows the apply is gated.
    #[test]
    fn loop_apply_honors_a_gating_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        // The pin and a clean recorded gate run, as `areev eval` writes them.
        let evalset = store
            .add(
                &Fact::new("evalset:gate", "mg:evalset", "{\"name\":\"gate\",\"cases\":[]}")
                    .namespace("agent:harness")
                    .created_at(1_000),
            )
            .unwrap()
            .to_hex();
        store
            .add(
                &Fact::new(
                    &format!("evalset:{evalset}"),
                    "mg:eval_run",
                    "{\"run_id\":\"eval-1\",\"passed\":3,\"failed\":0}",
                )
                .namespace("agent:harness")
                .created_at(2_000),
            )
            .unwrap();
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller").created_at(3_000)).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        // Register an adapter with real lineage (a recorded corpus export).
        let manifest = facade
            .record_corpus_export("RECALL facts", "train.jsonl", None, 4_000, &[], &[])
            .unwrap()
            .to_hex();
        facade
            .record_adapter(
                r#"{"adapter":{"uri":"file:///tmp/a","sha256":"feed"},"base_model":"qwen3-4b","serves_as":"acme-support"}"#,
                &manifest,
                &evalset,
                5_000,
            )
            .unwrap();
        // Token mode: the shared secret is the implied local owner — the
        // token-less console is read-only by design.
        let s = UiServer::new(facade, "test".into()).with_auth("gate-token".into());
        let tok = Some("gate-token");

        // Propose, then find the adapter rec — its row must carry the pin.
        let run = s.route("POST", "/api/loop/run", b"{}", tok, None);
        assert!(run.0.starts_with("200"), "{}", text(&run));
        let list = s.route("GET", "/api/loop/recommendations?status=pending", b"", tok, None);
        let rows: serde_json::Value = serde_json::from_str(&text(&list)).unwrap();
        let rec = rows["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["analyzer"].as_str().unwrap_or("").contains("adapter_intake"))
            .unwrap_or_else(|| panic!("adapter_intake must propose: {rows}"));
        assert_eq!(rec["evalset_hash"], evalset.as_str(), "the row must carry the pin");
        let hash = rec["hash"].as_str().unwrap().to_string();

        // Approve (the console's first POST), then: ungated apply refused,
        // a bad run id refused, the recorded run admits.
        let approve = format!(r#"{{"hash":"{hash}","decision":"approve","because":"reviewed"}}"#);
        let r = s.route("POST", "/api/loop/review", approve.as_bytes(), tok, None);
        assert!(text(&r).contains("\"ok\":true"), "{}", text(&r));
        let bare = format!(r#"{{"hash":"{hash}","because":"ship it"}}"#);
        let r = s.route("POST", "/api/loop/apply", bare.as_bytes(), tok, None);
        assert!(text(&r).contains("gating run"), "ungated must refuse: {}", text(&r));
        let bad = format!(r#"{{"hash":"{hash}","because":"ship","gating_run":"eval-nope"}}"#);
        let r = s.route("POST", "/api/loop/apply", bad.as_bytes(), tok, None);
        assert!(text(&r).contains("no recorded gate run"), "{}", text(&r));
        let good = format!(r#"{{"hash":"{hash}","because":"gated and green","gating_run":"eval-1"}}"#);
        let r = s.route("POST", "/api/loop/apply", good.as_bytes(), tok, None);
        assert!(text(&r).contains("\"ok\":true"), "gated apply failed: {}", text(&r));
    }

    /// A server in credential-map mode: two env-referenced tokens, rights
    /// seeded as grant grains in the file itself.
    fn server() -> UiServer {
        std::env::set_var("AREEV_TEST_READER_TOK", "reader-secret");
        std::env::set_var("AREEV_TEST_WRITER_TOK", "writer-secret");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        store
            .add(&Fact::new("user:reader", REL_PERMITS, "read ON *").namespace(AUTHZ_NS).created_at(1_000))
            .unwrap();
        store
            .add(
                &Fact::new("agent:writer", REL_PERMITS, "read,write ON caller")
                    .namespace(AUTHZ_NS)
                    .created_at(2_000),
            )
            .unwrap();
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller").created_at(3_000)).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"env":"AREEV_TEST_READER_TOK","principal":"user:reader"},
                {"env":"AREEV_TEST_WRITER_TOK","principal":"agent:writer"}
            ]}"#,
        )
        .unwrap();
        UiServer::new(facade, "test".into()).with_credentials(map)
    }

    fn text(r: &(&str, &str, Vec<u8>)) -> String {
        String::from_utf8_lossy(&r.2).to_string()
    }

    const READ: &[u8] = br#"{"query":"RECALL facts WHERE subject = \"acme\""}"#;
    const WRITE: &[u8] = br#"{"query":"ADD fact SET subject = \"x\" SET relation = \"r\" SET object = \"o\" SET namespace = \"caller\" REASON \"t\""}"#;

    #[test]
    fn tokens_bind_principals_and_verbs_decide() {
        let s = server();

        // The reader reads…
        let r = s.route("POST", "/api/cal", READ, Some("reader-secret"), None);
        assert!(r.0.starts_with("200"), "{}", text(&r));
        assert!(text(&r).contains("grains"));
        // …and cannot write.
        let w = s.route("POST", "/api/cal", WRITE, Some("reader-secret"), None);
        assert!(text(&w).contains("AUT-E001"), "reader write must be refused: {}", text(&w));

        // The writer writes.
        let w = s.route("POST", "/api/cal", WRITE, Some("writer-secret"), None);
        assert!(text(&w).contains("added"), "writer write must land: {}", text(&w));

        // Sequential isolation: the very next reader request is still
        // read-only — nothing leaked from the writer's binding.
        let w = s.route("POST", "/api/cal", WRITE, Some("reader-secret"), None);
        assert!(text(&w).contains("AUT-E001"), "{}", text(&w));
    }

    #[test]
    fn anonymous_is_read_only_and_loop_run_is_verb_gated() {
        let s = server();
        // No token → anonymous: reads work…
        let r = s.route("POST", "/api/cal", READ, None, None);
        assert!(r.0.starts_with("200"), "{}", text(&r));
        assert!(text(&r).contains("grains"));
        // …writes are verb-refused…
        let w = s.route("POST", "/api/cal", WRITE, None, None);
        assert!(text(&w).contains("AUT-E001"), "{}", text(&w));
        // …and an unknown token is anonymous too, never an escalation.
        let w = s.route("POST", "/api/cal", WRITE, Some("stolen-guess"), None);
        assert!(text(&w).contains("AUT-E001"), "{}", text(&w));

        // The analysis trigger needs loop.run — anonymous gets 403.
        let run = s.route("POST", "/api/loop/run", b"{}", None, None);
        assert!(run.0.starts_with("403"), "{}", text(&run));
    }

    #[test]
    fn whoami_reports_the_bound_principal() {
        let s = server();
        let r = s.route("GET", "/api/whoami", b"", Some("reader-secret"), None);
        assert!(text(&r).contains("user:reader"), "{}", text(&r));
        assert!(text(&r).contains("credentials"));
        let r = s.route("GET", "/api/whoami", b"", None, None);
        assert!(text(&r).contains("anonymous"), "{}", text(&r));
    }

    /// A credential map must not silently disable `--token-env`. Attaching
    /// one changes WHO a request is; the operator who also asked for a
    /// shared secret still gets authentication on every request.
    #[test]
    fn a_credential_map_does_not_disable_the_shared_secret() {
        let s = server().with_auth("console-token".into());

        // No credential at all: refused, not dropped to `anonymous`.
        let r = s.route("POST", "/api/cal", READ, None, None);
        assert!(r.0.starts_with("401"), "{}", text(&r));
        let page = s.route("GET", "/", b"", None, None);
        assert!(page.0.starts_with("401"), "the console page is guarded too");
        // An unrecognized token is refused rather than downgraded.
        let r = s.route("POST", "/api/cal", READ, Some("stolen-guess"), None);
        assert!(r.0.starts_with("401"), "{}", text(&r));

        // A map token still binds its principal and its verbs still decide.
        let r = s.route("POST", "/api/cal", READ, Some("reader-secret"), None);
        assert!(r.0.starts_with("200"), "{}", text(&r));
        let w = s.route("POST", "/api/cal", WRITE, Some("reader-secret"), None);
        assert!(text(&w).contains("AUT-E001"), "{}", text(&w));

        // The shared secret is the implied admin (docs/cal-all-you-need
        // -proposal.md §5.2b) — it keeps the owner session it always had.
        let w = s.route("POST", "/api/cal", WRITE, Some("console-token"), None);
        assert!(text(&w).contains("added"), "{}", text(&w));
    }

    /// `RUN LOOP` through CAL is gated exactly as `POST /api/loop/run` is —
    /// otherwise the CAL surface is a way around the HTTP gate.
    #[test]
    fn run_loop_via_cal_is_verb_gated_like_the_route() {
        let s = server();
        let run = s.route("POST", "/api/loop/run", b"{}", None, None);
        assert!(run.0.starts_with("403"), "{}", text(&run));
        let cal = s.route("POST", "/api/cal", br#"{"query":"RUN LOOP"}"#, None, None);
        assert!(
            text(&cal).contains("AUT-E001"),
            "anonymous must not run the loop through CAL either: {}",
            text(&cal)
        );
    }
}

#[cfg(test)]
mod namespace_route_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    fn text(r: &(&str, &str, Vec<u8>)) -> String {
        String::from_utf8_lossy(&r.2).to_string()
    }

    /// `agent:writer` is granted read+write on `caller` only — nothing on
    /// `other`, nothing on the audit namespace.
    fn restricted_server() -> UiServer {
        std::env::set_var("AREEV_TEST_NS_WRITER_TOK", "writer-secret");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        store
            .add(&Fact::new("agent:writer", REL_PERMITS, "read,write ON caller").namespace(AUTHZ_NS).created_at(1_000))
            .unwrap();
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller").created_at(2_000)).unwrap();
        store.add(&Fact::new("bob", "role", "admin").namespace("other").created_at(3_000)).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"AREEV_TEST_NS_WRITER_TOK","principal":"agent:writer"}]}"#,
        )
        .unwrap();
        UiServer::new(facade, "test".into()).with_credentials(map)
    }

    /// Unbound local open — the implicit owner, unrestricted on every
    /// namespace, same as a plain `areev ui` with no `--auth`.
    fn owner_server() -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller").created_at(1_000)).unwrap();
        store.add(&Fact::new("bob", "role", "admin").namespace("other").created_at(2_000)).unwrap();
        std::mem::forget(dir);
        UiServer::new(AreevFacade::with_session(store, Some("caller".into()), None), "t".into())
    }

    #[test]
    fn namespaces_route_hides_what_the_principal_cannot_read() {
        let s = restricted_server();
        let r = s.route("GET", "/api/namespaces", b"", Some("writer-secret"), None);
        let body = text(&r);
        assert!(body.contains("\"caller\""), "granted namespace must be listed: {body}");
        assert!(!body.contains("\"other\""), "ungranted namespace must not be listed: {body}");
        assert!(!body.contains(AUTHZ_NS), "the audit namespace must not leak either: {body}");
    }

    #[test]
    fn namespaces_route_lists_everything_for_the_owner() {
        let s = owner_server();
        let r = s.route("GET", "/api/namespaces", b"", None, None);
        let body = text(&r);
        assert!(body.contains("\"caller\"") && body.contains("\"other\""), "{body}");
    }

    #[test]
    fn browse_ns_param_is_read_gated_for_a_restricted_principal() {
        let s = restricted_server();
        let ok = s.route("GET", "/api/browse?ns=caller", b"", Some("writer-secret"), None);
        assert!(text(&ok).contains("\"ok\":true"), "their own granted namespace must work: {}", text(&ok));

        let denied = s.route("GET", "/api/browse?ns=other", b"", Some("writer-secret"), None);
        let body = text(&denied);
        assert!(body.contains("\"ok\":false"), "an ungranted namespace must be refused, not silently emptied: {body}");
    }

    #[test]
    fn browse_ns_star_requires_owner() {
        let s = restricted_server();
        let r = s.route("GET", "/api/browse?ns=*", b"", Some("writer-secret"), None);
        assert!(text(&r).contains("\"ok\":false"), "a restricted principal must not get the unfiltered view: {}", text(&r));

        let owner = owner_server();
        let r2 = owner.route("GET", "/api/browse?ns=*", b"", None, None);
        let body2 = text(&r2);
        assert!(body2.contains("\"ok\":true"), "the owner session may lift the filter: {body2}");
        assert!(body2.contains("\"caller\"") && body2.contains("\"other\""), "{body2}");
    }

    #[test]
    fn browse_without_ns_param_keeps_original_session_default_behavior() {
        let s = owner_server();
        let r = s.route("GET", "/api/browse", b"", None, None);
        let body = text(&r);
        assert!(body.contains("\"caller\""), "{body}");
        assert!(!body.contains("\"other\""), "omitting ?ns must behave exactly as before this feature existed: {body}");
    }
}

#[cfg(test)]
mod builder_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_store::Areev;

    fn server() -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Areev::open(path.to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        UiServer::new(AreevFacade::with_session(store, Some("caller".into()), None), "t".into())
    }

    /// `areev ui --no-destructive-ops --policy FILE` calls these in this
    /// order. A later builder must not re-enable destruction the operator
    /// turned off — the console was announced as read-only.
    #[test]
    fn attaching_a_loop_policy_preserves_the_destructive_cap() {
        let s = server()
            .allow_destructive_ops(false)
            .with_loop_policy(areev_loop::Policy::default());
        let r = s.route(
            "POST",
            "/api/cal",
            br#"{"query":"FORGET sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d"}"#,
            None,
            None,
        );
        let body = String::from_utf8_lossy(&r.2).to_string();
        assert!(
            body.contains("--no-destructive-ops") || r.0.starts_with("401"),
            "destruction must stay disabled: {body}"
        );
    }
}

#[cfg(test)]
mod run_api_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
    use areev_store::Areev;
    use std::sync::Arc;

    /// A server whose file holds a PARKED run (Client-gated approve node) —
    /// the HITL queue's fixture. Returns (server, ask id).
    fn parked_server() -> (UiServer, String) {
        std::env::set_var("AREEV_RUN_OFFICER_TOK", "officer-secret");
        std::env::set_var("AREEV_RUN_STARTER_TOK", "starter-secret");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        for (who, what) in [
            ("user:officer", "read,run.respond ON ops"),
            ("user:starter", "read ON ops"),
        ] {
            store
                .add(&Fact::new(who, REL_PERMITS, what).namespace(AUTHZ_NS).created_at(900))
                .unwrap();
        }
        std::mem::forget(dir);
        let facade = Arc::new(AreevFacade::with_session(store, Some("ops".into()), None));

        // Seed + run the plan to the park with a scripted runner.
        let (wf, _g, _a) = facade
            .with_store(|m| {
                let greet = Tool::new("greet")
                    .kind(ToolKind::Definition)
                    .tool_description("t")
                    .created_at(500)
                    .namespace("ops");
                let gh = m.add(&greet)?;
                let approve = Tool::new("approve")
                    .kind(ToolKind::Definition)
                    .tool_description("human approves")
                    .executor_kind(areev_core::types::ExecutorKind::Client)
                    .created_at(501)
                    .namespace("ops");
                let ah = m.add(&approve)?;
                let wf = Workflow::new(vec!["greet".into(), "approve".into()])
                    .edge("greet", "approve")
                    .bind("greet", &gh.to_hex())
                    .bind("approve", &ah.to_hex())
                    .created_at(502)
                    .namespace("ops");
                let wh = m.add(&wf)?;
                Ok::<_, areev_core::error::AreevError>((wh, gh, ah))
            })
            .unwrap();
        struct Ok1;
        impl areev_run::HostToolExecutor for Ok1 {
            fn execute(
                &self,
                t: &str,
                _h: &str,
                _i: &serde_json::Value,
                _k: &str,
            ) -> areev_run::ExecResult {
                areev_run::ExecResult::Ok(serde_json::json!({t.to_string(): true}))
            }
        }
        let runner = areev_run::Runner {
            facade: Arc::clone(&facade),
            clock: Arc::new(areev_run::ScriptedClock::new(
                (0..100).map(|i| 1_758_000_000_000 + i * 10).collect(),
            )),
            executor: Arc::new(Ok1),
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: "user:starter".into(),
        };
        let session = runner
            .start(&wf, "hitl-1", serde_json::json!({}), &areev_run::RunOptions {
                workers: 1,
                ..Default::default()
            })
            .unwrap();
        let areev_run::RunSession::Parked { envelope, .. } = session else {
            panic!("expected park")
        };
        let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();

        // Rebuild a facade-owning server over the SAME file is impossible
        // (single writer) — hand the server the same Arc'd facade via the
        // Arc-aware constructor path: UiServer::new takes AreevFacade, so
        // we build the server FIRST in real deployments; here we reuse the
        // internal Arc through a second UiServer field write. Instead:
        // drop the runner (facade Arc count drops) and unwrap.
        drop(runner);
        let facade = Arc::try_unwrap(facade).unwrap_or_else(|_| panic!("facade still shared"));
        let map = CredentialMap::from_json(
            r#"{"version":1,"tokens":[
                {"env":"AREEV_RUN_OFFICER_TOK","principal":"user:officer"},
                {"env":"AREEV_RUN_STARTER_TOK","principal":"user:starter"}
            ]}"#,
        )
        .unwrap();
        let server = UiServer::new(facade, "test".into())
            .with_auth("shared-console-secret".into())
            .with_credentials(map);
        (server, ask)
    }

    fn body(run_id: &str, ask: &str) -> Vec<u8> {
        serde_json::json!({"run_id": run_id, "tool_call_id": ask, "result": {"approved": true}})
            .to_string()
            .into_bytes()
    }

    /// #165: `/api/run/list` is scoped and paged on the server, and says
    /// its total and whether it truncated. Twenty older runs in a quiet
    /// namespace behind forty newer ones in a busy one is exactly the
    /// shape under which the old fixed newest-50 page, filtered
    /// client-side, showed the quiet tenant as having no runs.
    #[test]
    fn run_list_is_scoped_and_paged_server_side() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        for i in 0..60u32 {
            let (id, ns) = if i < 20 {
                (format!("quiet-{i}"), "tenant.quiet")
            } else {
                (format!("busy-{i}"), "tenant.busy")
            };
            let mut f = Fact::new(&format!("run:{id}"), "mg:harness", "00")
                .namespace(areev_core::authz::HARNESS_NS)
                .created_at(1_000 + i64::from(i));
            f.common.extra_fields.insert("run_id".into(), serde_json::json!(id));
            f.common.extra_fields.insert("run_ns".into(), serde_json::json!(ns));
            store.add(&f).unwrap();
        }
        std::mem::forget(dir);
        let s = UiServer::new(AreevFacade::with_session(store, Some("ops".into()), None), "test".into());
        let page = |query: &str| -> serde_json::Value {
            let r = s.route("GET", &format!("/api/run/list{query}"), b"", None, None);
            assert!(r.0.starts_with("200"), "{} {}", r.0, String::from_utf8_lossy(&r.2));
            serde_json::from_slice(&r.2).unwrap()
        };
        let rows = |v: &serde_json::Value| v["runs"].as_array().unwrap().clone();

        // The default page: newest fifty of sixty, and it SAYS so.
        let all = page("");
        assert_eq!(rows(&all).len(), 50);
        assert_eq!(all["total"], 60);
        assert_eq!(all["truncated"], true);
        assert_eq!(rows(&all).iter().filter(|r| r["ns"] == "tenant.quiet").count(), 10);

        // Scoped: the whole quiet tenant, no truncation, nothing else.
        let quiet = page("?ns=tenant.quiet");
        assert_eq!(quiet["total"], 20);
        assert_eq!(quiet["truncated"], false);
        assert_eq!(rows(&quiet).len(), 20);
        assert!(rows(&quiet).iter().all(|r| r["ns"] == "tenant.quiet"), "{quiet}");

        // A prefix scope, paged to the end.
        let tail = page("?ns=tenant.*&limit=10&offset=55");
        assert_eq!(tail["total"], 60);
        assert_eq!(rows(&tail).len(), 5);
        assert_eq!(tail["truncated"], false);
        assert_eq!((tail["offset"].as_u64(), tail["limit"].as_u64()), (Some(55), Some(10)));

        // `*` is unscoped; an absurd limit is clamped rather than honored.
        assert_eq!(page("?ns=*")["total"], 60);
        assert_eq!(page("?limit=100000")["limit"], 500);

        // A row from before the namespace stamp is not known to be in any
        // scope: excluded from a scoped read and COUNTED, present unscoped.
        s.facade
            .with_store(|m| {
                let mut f = Fact::new("run:old", "mg:harness", "00")
                    .namespace(areev_core::authz::HARNESS_NS)
                    .created_at(1);
                f.common.extra_fields.insert("run_id".into(), serde_json::json!("old"));
                m.add(&f)
            })
            .unwrap();
        let quiet = page("?ns=tenant.quiet");
        assert_eq!(quiet["total"], 20, "scoping stays exact: {quiet}");
        assert_eq!(quiet["unattributed"], 1, "and says what it left out: {quiet}");
        let unscoped = page("");
        assert_eq!(unscoped["total"], 61);
        assert_eq!(unscoped["unattributed"], 0);
    }

    #[test]
    fn shared_token_approvals_are_refused_and_principals_approve() {
        let (s, ask) = parked_server();

        // The HITL queue is visible.
        let r = s.route("GET", "/api/run/list", b"", Some("shared-console-secret"), None);
        assert!(r.0.starts_with("200"), "{}", String::from_utf8_lossy(&r.2));
        let r = s.route(
            "GET",
            "/api/run/inspect?run_id=hitl-1",
            b"",
            Some("shared-console-secret"),
            None,
        );
        assert!(String::from_utf8_lossy(&r.2).contains(&ask), "ask visible in inspect");

        // 1. The SHARED console secret cannot approve — identity is the point.
        let r = s.route("POST", "/api/run/respond", &body("hitl-1", &ask), Some("shared-console-secret"), None);
        assert!(r.0.starts_with("403"), "shared-token approval must refuse: {}", r.0);

        // 2. The STARTER's own credential cannot approve their own run
        //    (separation of duties, enforced by the runtime).
        let r = s.route("POST", "/api/run/respond", &body("hitl-1", &ask), Some("starter-secret"), None);
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains("responder") || text.contains("RUN-E012"), "{text}");

        // 3. The officer's principal-bound credential approves.
        let r = s.route("POST", "/api/run/respond", &body("hitl-1", &ask), Some("officer-secret"), None);
        assert!(r.0.starts_with("200"), "{}", String::from_utf8_lossy(&r.2));
        assert!(String::from_utf8_lossy(&r.2).contains("user:officer"));

        // 4. The kill switch keeps its low bar: shared token may cancel.
        let r = s.route(
            "POST",
            "/api/run/cancel",
            br#"{"run_id":"hitl-1","because":"test brake"}"#,
            Some("shared-console-secret"),
            None,
        );
        assert!(r.0.starts_with("200"), "{}", String::from_utf8_lossy(&r.2));
    }

    /// [A0] The approval trust floor. `user:officer` holds `run.respond ON
    /// ops` in the FILE, so the grants are identical whichever way the
    /// identity arrives — which is exactly the point. What differs is the
    /// PROOF: a credential-map token is a per-principal secret; an SSO header
    /// is trusted via one shared, fleet-wide proxy secret that lets its
    /// holder assert any approver. The default refuses the weaker proof.
    #[test]
    fn proxy_asserted_identities_cannot_approve_by_default() {
        let (s, ask) = parked_server();
        let s = s.with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>);

        // The SSO identity is a GRANTED principal and still cannot approve.
        let r = s.route("POST", "/api/run/respond", &body("hitl-1", &ask), None, Some("user:officer"));
        assert!(
            r.0.starts_with("403"),
            "proxy-asserted approval must refuse by default: {} {}",
            r.0,
            String::from_utf8_lossy(&r.2)
        );
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(
            text.contains("--sso-approvals"),
            "the refusal must name the flag that lifts it: {text}"
        );

        // Reviewing is unaffected — SSO identities keep every read.
        let r = s.route("GET", "/api/run/list", b"", None, Some("user:officer"));
        assert!(r.0.starts_with("200"), "SSO identities may still review: {}", r.0);

        // And the SAME principal, proven by its own credential, approves.
        let r = s.route("POST", "/api/run/respond", &body("hitl-1", &ask), Some("officer-secret"), None);
        assert!(r.0.starts_with("200"), "{}", String::from_utf8_lossy(&r.2));
    }

    /// [A0] `--sso-approvals allow` is the operator accepting that trade-off
    /// explicitly. Nothing else changes: the same identity, the same file
    /// grants, the same runtime separation-of-duties check behind it.
    #[test]
    fn sso_approvals_allow_opts_in_to_proxy_asserted_approvals() {
        let (s, ask) = parked_server();
        let s = s
            .with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>)
            .with_sso_approvals(true);

        let r = s.route("POST", "/api/run/respond", &body("hitl-1", &ask), None, Some("user:officer"));
        assert!(r.0.starts_with("200"), "{}", String::from_utf8_lossy(&r.2));
        assert!(String::from_utf8_lossy(&r.2).contains("user:officer"));
    }

    /// [A2] Groups → principals, so SSO scales without a grant grain per
    /// person — and the precedence that makes it safe: an identity with its
    /// own grants outranks its group, so adding a groups header can never
    /// narrow someone who already had rights.
    #[test]
    fn groups_resolve_to_principals_but_never_outrank_an_identity() {
        let (s, _ask) = parked_server();
        // `user:officer` is granted in the file; `role:reviewer` is not a
        // credential, only a group target.
        let map = CredentialMap::from_json(
            r#"{"version":1,
                "tokens":[{"env":"AREEV_RUN_OFFICER_TOK","principal":"user:officer","id":"off"}],
                "groups":{"Engineering":"user:officer"}}"#,
        )
        .unwrap();
        let s = s
            .with_credentials(map)
            .with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>)
            .with_sso_groups_header("x-forwarded-groups");

        // An UNGRANTED identity in a mapped group inherits the group's
        // principal — the whole point of the feature.
        let r = s.route_with_groups(
            "GET",
            "/api/whoami",
            b"",
            None,
            Some("user:newhire@example.com"),
            Some("platform,Engineering"),
        );
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains("user:officer"), "group should resolve: {text}");
        assert!(text.contains(r#""identity_source":"sso-group""#), "{text}");

        // Case-insensitive: directories are inconsistent, and a mapping that
        // misses on case would fail OPEN into the identity's own (empty)
        // grants.
        let r = s.route_with_groups(
            "GET", "/api/whoami", b"", None, Some("user:newhire@example.com"), Some("ENGINEERING"),
        );
        assert!(String::from_utf8_lossy(&r.2).contains("user:officer"));

        // An identity that IS granted keeps its own principal, group or not.
        let r = s.route_with_groups(
            "GET", "/api/whoami", b"", None, Some("user:officer"), Some("Engineering"),
        );
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains(r#""identity_source":"sso""#), "identity outranks group: {text}");

        // An unmapped group resolves to nothing and the identity stands.
        let r = s.route_with_groups(
            "GET", "/api/whoami", b"", None, Some("user:newhire@example.com"), Some("sales"),
        );
        assert!(String::from_utf8_lossy(&r.2).contains("user:newhire@example.com"));
    }

    /// [A2] A role is not an approver. This refusal has no flag of its own —
    /// `--sso-approvals allow` does not lift it — because an audit record
    /// reading "role:eng approved" names nobody who can be asked why.
    #[test]
    fn group_derived_principals_can_never_approve() {
        let (s, ask) = parked_server();
        let map = CredentialMap::from_json(
            r#"{"version":1,
                "tokens":[{"env":"AREEV_RUN_OFFICER_TOK","principal":"user:officer","id":"off"}],
                "groups":{"approvers":"user:officer"}}"#,
        )
        .unwrap();
        let s = s
            .with_credentials(map)
            .with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>)
            .with_sso_groups_header("x-forwarded-groups")
            // Even with the A0 opt-in explicitly granted.
            .with_sso_approvals(true);

        let r = s.route_with_groups(
            "POST",
            "/api/run/respond",
            &body("hitl-1", &ask),
            None,
            Some("user:newhire@example.com"),
            Some("approvers"),
        );
        assert!(r.0.starts_with("403"), "{} {}", r.0, String::from_utf8_lossy(&r.2));
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains("role"), "the refusal must say why: {text}");
    }

    /// [A2] The prefix keeps IdP-sourced principals visibly distinct from
    /// local ones everywhere they land.
    #[test]
    fn the_principal_prefix_is_stamped_on_proxy_asserted_identities() {
        let (s, _ask) = parked_server();
        let s = s
            .with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>)
            .with_sso_principal_prefix("sso:");

        let r = s.route("GET", "/api/whoami", b"", None, Some("pat@example.com"));
        assert!(String::from_utf8_lossy(&r.2).contains("sso:pat@example.com"), "{}", String::from_utf8_lossy(&r.2));

        // Idempotent: an identity the proxy already prefixed is not
        // double-stamped into a different principal than the grant names.
        let r = s.route("GET", "/api/whoami", b"", None, Some("sso:pat@example.com"));
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains("sso:pat@example.com"), "{text}");
        assert!(!text.contains("sso:sso:"), "must not double-stamp: {text}");
    }

    /// [A0] `whoami` tells an approver where they stand BEFORE they try to
    /// approve — a console that only discovers this at the 403 is a console
    /// that discovers it mid-incident.
    #[test]
    fn whoami_reports_identity_provenance_and_approval_capability() {
        let (s, _ask) = parked_server();
        let s = s.with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>);

        let r = s.route("GET", "/api/whoami", b"", None, Some("user:officer"));
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains(r#""identity_source":"sso""#), "{text}");
        assert!(text.contains(r#""may_approve":false"#), "{text}");

        let r = s.route("GET", "/api/whoami", b"", Some("officer-secret"), None);
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains(r#""identity_source":"credential""#), "{text}");
        assert!(text.contains(r#""may_approve":true"#), "{text}");
    }
}

#[cfg(test)]
mod memory_scoped_token_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    /// The enterprise-plane rule: one auth file, per-memory token
    /// scopes — a token listed for memory A is an UNKNOWN token on
    /// memory B's server.
    #[test]
    fn memory_scoped_tokens_do_not_cross_memories() {
        std::env::set_var("AREEV_SCOPED_TOK", "scoped-secret");
        let map_json = r#"{"version":1,"tokens":[
            {"env":"AREEV_SCOPED_TOK","principal":"agent:sync","memories":["memory-a"]}
        ]}"#;
        let server_for = |label: &str| {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
            store
                .add(
                    &Fact::new("agent:sync", REL_PERMITS, "read,write ON caller")
                        .namespace(AUTHZ_NS)
                        .created_at(1_000),
                )
                .unwrap();
            store
                .add(&Fact::new("acme", "tier", "Enterprise").namespace("caller").created_at(2_000))
                .unwrap();
            std::mem::forget(dir);
            let facade = AreevFacade::with_session(store, Some("caller".into()), None);
            UiServer::new(facade, label.into())
                .with_credentials(CredentialMap::from_json(map_json).unwrap())
        };
        const WRITE: &[u8] = br#"{"query":"ADD fact SET subject = \"x\" SET relation = \"r\" SET object = \"o\" SET namespace = \"caller\" REASON \"t\""}"#;

        // On memory-a the token authenticates and its grants let the write
        // through (no authorization refusal in the CAL payload).
        let a = server_for("memory-a");
        let r = a.route("POST", "/api/cal", WRITE, Some("scoped-secret"), None);
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(r.0.starts_with("200") && !text.contains("AUT-E001"), "{text}");

        // On memory-b the SAME token is an unknown credential: the request
        // runs as anonymous and the write is refused (AUT-E001 rides the
        // CAL payload — the console surfaces statement results in-band).
        let b = server_for("memory-b");
        let r = b.route("POST", "/api/cal", WRITE, Some("scoped-secret"), None);
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(
            text.contains("AUT-E001") && text.contains("anonymous"),
            "scoped token must not cross memories: {text}"
        );

        // An auth file with an EMPTY memories scope refuses to load.
        assert!(CredentialMap::from_json(
            r#"{"version":1,"tokens":[{"env":"AREEV_SCOPED_TOK","principal":"x","memories":[]}]}"#
        )
        .is_err());
    }
}

#[cfg(all(test, feature = "tls"))]
mod tls_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_store::Areev;
    use std::io::{Read, Write};

    fn self_signed(dir: &std::path::Path) -> Option<(String, String)> {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", key.to_str().unwrap(), "-out", cert.to_str().unwrap(),
                "-days", "1", "-subj", "/CN=localhost",
                "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
                "-addext", "basicConstraints=critical,CA:FALSE",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => Some((
                cert.to_str().unwrap().to_string(),
                key.to_str().unwrap().to_string(),
            )),
            _ => None, // no openssl on this host: the CI leg covers it
        }
    }

    /// The §3.2 gate: a TLS round-trip succeeds, and a PLAINTEXT client on
    /// the same listener is refused (no silent downgrade).
    #[test]
    fn tls_round_trip_and_no_plaintext_downgrade() {
        let dir = tempfile::tempdir().unwrap();
        let Some((cert, key)) = self_signed(dir.path()) else {
            eprintln!("skipping: no openssl binary");
            return;
        };
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let server = UiServer::new(facade, "tls-test".into())
            .with_tls(&cert, &key)
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            // Two connections: the TLS client, then the plaintext prober.
            for _ in 0..2 {
                if let Ok((s, _)) = listener.accept() {
                    let _ = server.handle(s);
                }
            }
        });

        // 1. TLS round-trip: trust exactly our self-signed cert.
        let mut roots = rustls::RootCertStore::empty();
        {
            use rustls::pki_types::pem::PemObject;
            for c in rustls::pki_types::CertificateDer::pem_file_iter(&cert).unwrap() {
                roots.add(c.unwrap()).unwrap();
            }
        }
        let config = std::sync::Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let session = rustls::ClientConnection::new(config, name).unwrap();
        let tcp = std::net::TcpStream::connect(addr).unwrap();
        let mut tls = rustls::StreamOwned::new(session, tcp);
        tls.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        let _ = tls.read_to_string(&mut response);
        assert!(response.starts_with("HTTP/1.1 200"), "{}", &response[..response.len().min(120)]);
        assert!(response.contains("Areev"), "console served over TLS");

        // 2. No downgrade: plaintext HTTP on the TLS listener gets a
        // handshake failure, never a page.
        let mut plain = std::net::TcpStream::connect(addr).unwrap();
        plain
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let _ = plain.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        assert!(
            !text.contains("HTTP/1.1 200"),
            "plaintext must not be served on a TLS listener: {text}"
        );
        handle.join().unwrap();
    }

    /// `with_tls` composes with `with_auth` — `with_auth` only sets the
    /// token/auth_all fields, not the connection layer, so an authenticated
    /// console terminates TLS identically to an open one. Pins the CLI
    /// wiring that lets `areev ui --token-env VAR --tls-cert ... --tls-key
    /// ...` work, not just `areev ui --tls-cert ...`.
    #[test]
    fn an_authenticated_console_terminates_tls_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let Some((cert, key)) = self_signed(dir.path()) else {
            eprintln!("skipping: no openssl binary");
            return;
        };
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let server = UiServer::new(facade, "auth-tls-test".into())
            .with_auth("console-token".into())
            .with_tls(&cert, &key)
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                let _ = server.handle(s);
            }
        });

        let mut roots = rustls::RootCertStore::empty();
        {
            use rustls::pki_types::pem::PemObject;
            for c in rustls::pki_types::CertificateDer::pem_file_iter(&cert).unwrap() {
                roots.add(c.unwrap()).unwrap();
            }
        }
        let config = std::sync::Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let session = rustls::ClientConnection::new(config, name).unwrap();
        let tcp = std::net::TcpStream::connect(addr).unwrap();
        let mut tls = rustls::StreamOwned::new(session, tcp);
        tls.write_all(
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\
              Authorization: Bearer console-token\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
        let mut response = String::new();
        let _ = tls.read_to_string(&mut response);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "an authenticated console should terminate TLS like an open one: {}",
            &response[..response.len().min(120)]
        );
        handle.join().unwrap();
    }
}

#[cfg(test)]
mod sso_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;
    use std::io::{Read, Write};

    const WRITE: &[u8] = br#"{"query":"ADD fact SET subject = \"x\" SET relation = \"r\" SET object = \"o\" SET namespace = \"caller\" REASON \"t\""}"#;

    fn sso_server() -> UiServer {
        sso_server_with("proxy-secret", None)
    }

    /// The same server mid-rotation: `proxy-secret` deployed, `next` incoming.
    fn sso_server_rotating(next: Option<&str>) -> UiServer {
        sso_server_with("proxy-secret", next)
    }

    fn sso_server_with(secret: &str, next: Option<&str>) -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        store
            .add(
                &Fact::new("user:pat@example.com", REL_PERMITS, "read,write ON caller")
                    .namespace(AUTHZ_NS)
                    .created_at(1_000),
            )
            .unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        UiServer::new(facade, "sso-test".into())
            .with_sso_rotating("x-forwarded-user", secret, next)
    }

    fn post(server: &UiServer, extra_headers: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Ok((s, _)) = listener.accept() {
                    let _ = server.handle(s);
                }
            });
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let req = format!(
                "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                WRITE.len()
            );
            c.write_all(req.as_bytes()).unwrap();
            c.write_all(WRITE).unwrap();
            let mut out = String::new();
            let _ = c.read_to_string(&mut out);
            out
        })
    }

    /// The §3.3 gate: a proxy-proven identity lands as a named actor whose
    /// FILE grants decide; a forged identity header without the proxy
    /// secret is ignored (the write is refused as anonymous).
    #[test]
    fn proxied_identity_works_and_forged_headers_are_ignored() {
        let server = sso_server();

        // 1. Proxy-proven: identity + secret → the granted principal writes.
        let ok = post(
            &server,
            "X-Forwarded-User: user:pat@example.com\r\nX-Areev-Proxy-Secret: proxy-secret\r\n",
        );
        assert!(ok.contains("HTTP/1.1 200"), "{}", &ok[..ok.len().min(200)]);
        assert!(!ok.contains("AUT-E001"), "granted principal must write: {ok}");

        // 2. Forged: the identity header WITHOUT the secret is ignored — the
        // request is anonymous and the write refuses.
        let forged = post(&server, "X-Forwarded-User: user:pat@example.com\r\n");
        assert!(
            forged.contains("anonymous") || forged.contains("401"),
            "forged identity must not authenticate: {forged}"
        );

        // 3. Wrong secret: same refusal (constant-time compare, same path).
        let wrong = post(
            &server,
            "X-Forwarded-User: user:pat@example.com\r\nX-Areev-Proxy-Secret: guess\r\n",
        );
        assert!(
            wrong.contains("anonymous") || wrong.contains("401"),
            "wrong proxy secret must not authenticate: {wrong}"
        );
    }

    /// [A2] The proxy secret proves the PROXY. It does not make whatever the
    /// proxy forwarded well-formed — and an identity lands in immutable,
    /// replicating audit grains, so a malformed one outlives its request.
    #[test]
    fn malformed_identities_are_rejected_even_with_a_valid_proxy_secret() {
        assert_eq!(
            super::sanitize_sso_identity("  user:pat@example.com  ").as_deref(),
            Some("user:pat@example.com"),
            "surrounding whitespace is trimmed, not rejected"
        );

        // CR/LF would smuggle a second line into every log and audit record.
        assert!(super::sanitize_sso_identity("user:a\r\nX-Admin: true").is_none());
        assert!(super::sanitize_sso_identity("user:a\tb").is_none());
        // Internal whitespace would make "user: a" and "user:a" two audit
        // identities for one person.
        assert!(super::sanitize_sso_identity("user a").is_none());
        assert!(super::sanitize_sso_identity("").is_none());
        assert!(super::sanitize_sso_identity(&"x".repeat(129)).is_none());
        // Reserved: an audit line must never be ambiguous between a vouched
        // person and the console's own floor/ceiling principals.
        assert!(super::sanitize_sso_identity("anonymous").is_none());
        assert!(super::sanitize_sso_identity("user:console").is_none());
    }

    /// [A2] A rejected identity is treated as an ABSENT one — the request
    /// degrades to anonymous rather than the console refusing to serve, so a
    /// misconfigured proxy cannot take the console down.
    #[test]
    fn a_rejected_identity_degrades_to_anonymous_rather_than_erroring() {
        let server = sso_server();
        let out = post(
            &server,
            "X-Forwarded-User: anonymous\r\nX-Areev-Proxy-Secret: proxy-secret\r\n",
        );
        assert!(
            out.contains("anonymous") || out.contains("401"),
            "a reserved name must not authenticate: {out}"
        );
        assert!(!out.contains("HTTP/1.1 500"), "and must not error: {out}");
    }

    /// The rotation window (#79): while two secrets are configured, either
    /// proves the proxy — so the fleet moves over one node at a time instead
    /// of atomically, which is the only way an impersonation-grade credential
    /// gets rotated without an outage or a gap.
    #[test]
    fn both_secrets_authenticate_during_a_rotation_window() {
        let server = sso_server_rotating(Some("new-secret"));
        let id = "X-Forwarded-User: user:pat@example.com\r\n";

        for secret in ["proxy-secret", "new-secret"] {
            let out = post(&server, &format!("{id}X-Areev-Proxy-Secret: {secret}\r\n"));
            assert!(out.contains("HTTP/1.1 200"), "{secret} rejected: {out}");
            assert!(!out.contains("AUT-E001"), "{secret} must authenticate: {out}");
        }

        // The window widens what proves the proxy, never what a proved
        // identity may do, and never to a third value.
        let wrong = post(&server, &format!("{id}X-Areev-Proxy-Secret: guess\r\n"));
        assert!(
            wrong.contains("anonymous") || wrong.contains("401"),
            "an unrelated secret must still be refused mid-rotation: {wrong}"
        );
    }

    /// Closing the window is what actually retires the old credential — the
    /// step the runbook exists to make sure nobody skips.
    #[test]
    fn the_retired_secret_stops_working_once_the_window_closes() {
        // Rotation complete: the new value is now the only one deployed.
        let server = sso_server_with("new-secret", None);
        let id = "X-Forwarded-User: user:pat@example.com\r\n";

        let ok = post(&server, &format!("{id}X-Areev-Proxy-Secret: new-secret\r\n"));
        assert!(ok.contains("HTTP/1.1 200"), "{ok}");
        assert!(!ok.contains("AUT-E001"), "the promoted secret must authenticate: {ok}");

        let old = post(&server, &format!("{id}X-Areev-Proxy-Secret: proxy-secret\r\n"));
        assert!(
            old.contains("anonymous") || old.contains("401"),
            "the retired secret must stop working: {old}"
        );
    }

    #[test]
    #[should_panic(expected = "must differ")]
    fn rotating_to_the_same_value_is_refused() {
        // A rotation that deploys the same value twice did not happen, and it
        // would read as "both live" in every log and status line.
        let _ = sso_server_rotating(Some("proxy-secret"));
    }

    use crate::CONSOLE_HTML;
    use std::collections::BTreeSet;

    /// Every `<section class="page" id="page-X">` must be named in the one
    /// array in `render()` that clears `hidden`, and every sidebar
    /// `data-page="X"` must have such a section.
    ///
    /// The console's page sections ship `hidden` in the markup, so a page is
    /// visible only if `render()` un-hides it. The Triggers tab shipped
    /// missing from that array: `renderTriggers()` filled `#trgBody` on every
    /// render, the nav item highlighted, the hash routed — and the section
    /// stayed `hidden` while every other page was hidden too, so the tab
    /// showed a blank pane. Nothing else in the file could catch it: the
    /// omission is a *missing* string, not a wrong one.
    #[test]
    fn every_console_page_is_in_the_visibility_list() {
        fn all(hay: &str, open: &str) -> BTreeSet<String> {
            let mut out = BTreeSet::new();
            let mut rest = hay;
            while let Some(i) = rest.find(open) {
                rest = &rest[i + open.len()..];
                let end = rest.find('"').expect("unterminated attribute");
                out.insert(rest[..end].to_string());
                rest = &rest[end..];
            }
            out
        }

        let sections = all(CONSOLE_HTML, "id=\"page-");
        let nav = all(CONSOLE_HTML, "data-page=\"");
        for want in ["workflows", "runs", "tools"] {
            assert!(sections.contains(want), "the {want} section itself went missing");
        }
        // Triggers deliberately have no page: a trigger points AT a plan and is
        // only legible next to it, so they render in the workflow canvas's own
        // lane. The old deep link must still land somewhere real.
        assert!(!sections.contains("triggers"), "triggers belong on the canvas, not on a page");
        assert!(
            CONSOLE_HTML.contains("if (head === 'triggers')"),
            "#triggers must still route somewhere — a removed page is not a dead bookmark"
        );

        // The single array in render() that drives `.hidden`.
        let head = "for (const p of [";
        let i = CONSOLE_HTML.find(head).expect("render()'s page-visibility loop");
        let tail = &CONSOLE_HTML[i + head.len()..];
        let j = tail.find(']').expect("unterminated page array");
        let listed: BTreeSet<String> = tail[..j]
            .split(',')
            .map(|t| t.trim().trim_matches('\'').to_string())
            .filter(|t| !t.is_empty())
            .collect();

        assert_eq!(
            sections, listed,
            "page sections and render()'s visibility list disagree; a section missing \
             from the list renders into a pane that never un-hides"
        );
        let orphan: Vec<_> = nav.difference(&sections).collect();
        assert!(orphan.is_empty(), "nav items with no page section: {orphan:?}");
    }
}

/// Issue #124: the console must never disclose the password half of a
/// Postgres DSN, at any of its three display surfaces, while the RAW label
/// keeps working as the credential map's `memories` comparison key.
#[cfg(test)]
mod dsn_redaction_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    const DSN: &str = "postgresql://u:SUPERSECRET@h:5432/d?sslmode=require";

    fn server() -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        UiServer::new(facade, DSN.to_string())
    }

    fn text(r: &(&str, &str, Vec<u8>)) -> String {
        String::from_utf8_lossy(&r.2).to_string()
    }

    #[test]
    fn stats_config_and_html_redact_the_dsn_password_but_keep_the_host() {
        let s = server();
        for (method, path) in [("GET", "/api/stats"), ("GET", "/api/config"), ("GET", "/")] {
            let r = s.route(method, path, b"", None, None);
            let body = text(&r);
            assert!(!body.contains("SUPERSECRET"), "{path} leaked the password: {body}");
            assert!(body.contains("h:5432"), "{path} redacted more than the password: {body}");
        }
    }

    /// Redaction is DISPLAY ONLY: a credential map's `memories` entry is
    /// documented as the raw `--db` value, and it must still resolve —
    /// proving the redacted form never leaked into the auth-comparison
    /// path (`resolve_for_memory`'s exact string compare).
    #[test]
    fn credential_map_memories_entry_still_matches_the_raw_dsn() {
        std::env::set_var("AREEV_DSN_TOK", "dsn-scoped-secret");
        let map_json = format!(
            r#"{{"version":1,"tokens":[{{"env":"AREEV_DSN_TOK","principal":"agent:sync","memories":["{DSN}"]}}]}}"#
        );
        let dir = tempfile::tempdir().unwrap();
        let mut store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        store
            .add(
                &Fact::new("agent:sync", REL_PERMITS, "read,write ON caller")
                    .namespace(AUTHZ_NS)
                    .created_at(1_000),
            )
            .unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let s = UiServer::new(facade, DSN.to_string())
            .with_credentials(CredentialMap::from_json(&map_json).unwrap());

        const WRITE: &[u8] = br#"{"query":"ADD fact SET subject = \"x\" SET relation = \"r\" SET object = \"o\" SET namespace = \"caller\" REASON \"t\""}"#;
        let r = s.route("POST", "/api/cal", WRITE, Some("dsn-scoped-secret"), None);
        let body = text(&r);
        assert!(r.0.starts_with("200") && !body.contains("AUT-E001"), "{body}");
    }
}

/// Issue #125: `--allow-origin` lets an exact origin's cross-origin POSTs
/// through, without weakening the check for anyone else — no wildcards, no
/// suffix/subdomain matching. The Origin header is parsed in
/// `handle_request` (not `route`), so these go over a real socket.
#[cfg(test)]
mod origin_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_store::Areev;
    use std::io::{Read, Write};

    fn server() -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        UiServer::new(facade, "origin-test".into())
    }

    /// POST a read-only CAL query (works token-less on an open console) with
    /// an optional `Origin` header, returning the raw HTTP response text.
    fn post_with_origin(server: &UiServer, origin: Option<&str>) -> String {
        let body: &[u8] = br#"{"query":"RECALL facts WHERE subject = \"acme\""}"#;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Ok((s, _)) = listener.accept() {
                    let _ = server.handle(s);
                }
            });
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let origin_header = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
            let req = format!(
                "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{origin_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            c.write_all(req.as_bytes()).unwrap();
            c.write_all(body).unwrap();
            let mut out = String::new();
            let _ = c.read_to_string(&mut out);
            out
        })
    }

    #[test]
    fn matching_allowed_origin_is_accepted() {
        let s = server().allow_origin("https://console.example.com");
        let r = post_with_origin(&s, Some("https://console.example.com"));
        assert!(r.contains("HTTP/1.1 200"), "{r}");
    }

    #[test]
    fn non_matching_origin_is_still_rejected() {
        let s = server().allow_origin("https://console.example.com");
        let r = post_with_origin(&s, Some("https://not-allowed.example.com"));
        assert!(r.contains("403"), "{r}");
        assert!(r.contains("cross-origin request rejected"), "{r}");
    }

    #[test]
    fn a_subdomain_of_an_allowed_origin_is_rejected_not_matched() {
        // The allowlist entry is the console's real origin; a lookalike
        // subdomain must not be accepted by suffix matching.
        let s = server().allow_origin("https://console.example.com");
        let r = post_with_origin(&s, Some("https://evil-console.example.com"));
        assert!(r.contains("403"), "{r}");
    }

    #[test]
    fn loopback_origin_still_works_with_no_allowlist_set() {
        let s = server();
        let r = post_with_origin(&s, Some("http://127.0.0.1:7437"));
        assert!(r.contains("HTTP/1.1 200"), "{r}");
    }

    #[test]
    fn no_origin_header_still_works() {
        let s = server();
        let r = post_with_origin(&s, None);
        assert!(r.contains("HTTP/1.1 200"), "{r}");
    }

    #[test]
    fn matching_origin_differing_only_in_case_and_trailing_slash_is_accepted() {
        let s = server().allow_origin("https://console.example.com");
        let r = post_with_origin(&s, Some("HTTPS://Console.Example.com/"));
        assert!(r.contains("HTTP/1.1 200"), "{r}");
    }
}

/// Issue #126: repeated bad credentials from one source IP are COUNTED and
/// LOGGED in a stable, greppable shape — but never delayed or locked out
/// in-process (`serve` is a strictly serial accept loop; see
/// `note_auth_failure`'s doc comment for why a sleep here would itself be a
/// denial-of-service lever). A request that never presented a credential at
/// all has nothing to fail and is never counted.
/// [A3] Native OIDC at the request level: cookies, the `/auth/*` endpoints,
/// and the approval rules an IdP-proven identity is subject to.
#[cfg(all(test, feature = "oidc"))]
mod oidc_route_tests {
    use super::{cookie_value, oidc, UiServer};
    use areev_cal::AreevFacade;
    use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    fn cfg() -> oidc::OidcConfig {
        oidc::OidcConfig {
            issuer: "https://idp.example.com".into(),
            client_id: "console".into(),
            client_secret: "s3cr3t".into(),
            redirect_uri: "https://console.example.com/auth/callback".into(),
            scopes: "openid email".into(),
            principal_claim: "email".into(),
            principal_prefix: None,
        }
    }

    /// A cookie header is attacker-influenced: any script on any same-site
    /// origin can set cookies. A prefix or substring match would let a decoy
    /// shadow the real session.
    #[test]
    fn cookie_lookup_is_exact_on_the_name() {
        let h = "theme=dark; areev_session=real; other=x";
        assert_eq!(cookie_value(h, "areev_session").as_deref(), Some("real"));
        // A decoy whose name merely CONTAINS the real one must not match.
        let decoy = "areev_session_decoy=evil; areev_session=real";
        assert_eq!(cookie_value(decoy, "areev_session").as_deref(), Some("real"));
        let only_decoy = "xareev_session=evil; areev_sessionx=evil2";
        assert!(cookie_value(only_decoy, "areev_session").is_none());
        assert!(cookie_value("", "areev_session").is_none());
    }

    /// `/auth/login` redirects to the IdP and stores nothing in the browser;
    /// `/auth/logout` clears the cookie AND drops the server-side session.
    #[test]
    fn the_auth_endpoints_redirect_and_manage_the_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("ops".into()), None);
        let provider = oidc::OidcProvider::with_test_session(cfg(), "sid-1", "user:pat");
        let s = UiServer::new(facade, "oidc-test".into()).with_oidc(provider);

        // login → 302 to the IdP, and no Set-Cookie (nothing to remember yet).
        let (status, extra, _) = s.oidc_route("GET", "/auth/login", None);
        assert!(status.starts_with("302"), "{status}");
        assert!(extra.contains("Location: https://idp.example.com/authorize"), "{extra}");
        assert!(!extra.contains("Set-Cookie"), "{extra}");
        assert!(extra.contains("Cache-Control: no-store"), "a redirect carrying state must not cache: {extra}");

        // The session works before logout...
        assert!(s.oidc_principal(Some("areev_session=sid-1")).is_some());

        // ...and logout clears the browser copy AND invalidates server-side.
        let (status, extra, _) =
            s.oidc_route("POST", "/auth/logout", Some("areev_session=sid-1"));
        assert!(status.starts_with("302"), "{status}");
        assert!(extra.contains("Max-Age=0"), "{extra}");
        assert!(extra.contains("HttpOnly"), "{extra}");
        assert!(extra.contains("SameSite=Strict"), "{extra}");
        assert!(
            s.oidc_principal(Some("areev_session=sid-1")).is_none(),
            "logout must invalidate the session, not just the browser's copy"
        );

        // A callback with no code surfaces the IdP's own error.
        let (status, _, body) =
            s.oidc_route("GET", "/auth/callback?error=access_denied", None);
        assert!(status.starts_with("400"), "{status}");
        assert!(body.contains("access_denied"), "{body}");
    }

    /// The cookie is `HttpOnly` + `SameSite=Strict`, and `Secure` exactly
    /// when the redirect URI is https — a loopback console over plain http
    /// must still be able to log in.
    #[test]
    fn the_session_cookie_is_httponly_strict_and_secure_only_over_https() {
        let mk = |redirect: &str| {
            let dir = tempfile::tempdir().unwrap();
            let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
            std::mem::forget(dir);
            let facade = AreevFacade::with_session(store, Some("ops".into()), None);
            let mut c = cfg();
            c.redirect_uri = redirect.into();
            let p = oidc::OidcProvider::with_test_session(c, "sid", "user:pat");
            UiServer::new(facade, "t".into()).with_oidc(p)
        };

        let https = mk("https://console.example.com/auth/callback");
        let (_, extra, _) = https.oidc_route("GET", "/auth/logout", None);
        assert!(extra.contains("; Secure"), "https deployment must set Secure: {extra}");

        let http = mk("http://127.0.0.1:7437/auth/callback");
        let (_, extra, _) = http.oidc_route("GET", "/auth/logout", None);
        assert!(!extra.contains("; Secure"), "loopback http must not set Secure: {extra}");
        // The other two are unconditional.
        assert!(extra.contains("HttpOnly") && extra.contains("SameSite=Strict"), "{extra}");
    }

    /// The point of the whole feature: an OIDC-proven identity MAY approve,
    /// and `--sso-approvals` (which governs the header path) does not touch
    /// it. A signature verified against the issuer's key set is a stronger
    /// claim than a shared proxy secret, so it gets the stronger right.
    #[test]
    fn an_oidc_principal_may_approve_regardless_of_sso_approvals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = Areev::open(path.to_str().unwrap()).unwrap();
        store
            .add(
                &Fact::new("user:pat", REL_PERMITS, "read,run.respond ON ops")
                    .namespace(AUTHZ_NS)
                    .created_at(900),
            )
            .unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("ops".into()), None);
        let provider = oidc::OidcProvider::with_test_session(cfg(), "sid-1", "user:pat");
        let s = UiServer::new(facade, "oidc-test".into())
            .with_oidc(provider)
            // The A0 default, explicitly: SSO headers may NOT approve.
            .with_sso_approvals(false);

        let r = s.route_full("GET", "/api/whoami", b"", None, None, None, Some("user:pat"));
        let text = String::from_utf8_lossy(&r.2).to_string();
        assert!(text.contains(r#""identity_source":"oidc""#), "{text}");
        assert!(
            text.contains(r#""may_approve":true"#),
            "an IdP-proven identity is exactly what --sso-approvals exists to \
             substitute for; it must not be caught by that refusal: {text}"
        );
        assert!(text.contains("user:pat"), "{text}");
    }
}

#[cfg(test)]
mod auth_failure_tracking_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_store::Areev;
    use std::io::{Read, Write};
    use std::net::IpAddr;

    const LOCALHOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    fn server() -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        UiServer::new(facade, "auth-failure-test".into()).with_auth("right-token".into())
    }

    /// GET `/` with an optional `Authorization: Bearer` header, over a real
    /// socket (the counter hooks `handle_request`, not `route`).
    fn get_root(server: &UiServer, bearer: Option<&str>) -> String {
        let auth_header = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        get_root_with(server, &auth_header)
    }

    /// `GET /` with arbitrary extra headers, over a real socket (the counter
    /// hooks `handle_request`, not `route`).
    fn get_root_with(server: &UiServer, extra_headers: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Ok((s, _)) = listener.accept() {
                    let _ = server.handle(s);
                }
            });
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let req = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}Connection: close\r\n\r\n");
            c.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = c.read_to_string(&mut out);
            out
        })
    }

    #[test]
    fn a_bare_request_is_401_but_not_counted() {
        let s = server();
        // No credential presented at all: 401 (auth_all guards every
        // request), but nothing was attempted, so it must not be counted —
        // a token-less probe is not an attack attempt.
        let bare = get_root(&s, None);
        assert!(bare.contains("HTTP/1.1 401"), "{bare}");
        assert_eq!(s.auth_failure_count_for_test(LOCALHOST), 0, "a bare request has nothing to fail");
    }

    #[test]
    fn a_wrong_token_is_401_and_is_counted() {
        let s = server();
        let wrong = get_root(&s, Some("guess"));
        assert!(wrong.contains("HTTP/1.1 401"), "{wrong}");
        assert_eq!(s.auth_failure_count_for_test(LOCALHOST), 1);

        let wrong_again = get_root(&s, Some("guess"));
        assert!(wrong_again.contains("HTTP/1.1 401"), "{wrong_again}");
        assert_eq!(s.auth_failure_count_for_test(LOCALHOST), 2, "consecutive failures accumulate");
    }

    #[test]
    fn a_right_token_after_a_streak_succeeds_immediately_and_resets_it() {
        let s = server();
        for _ in 0..5 {
            let r = get_root(&s, Some("guess"));
            assert!(r.contains("HTTP/1.1 401"), "{r}");
        }
        assert_eq!(s.auth_failure_count_for_test(LOCALHOST), 5);

        // The right token succeeds on the very next request — there is no
        // in-process DELAY at any streak length, and below
        // MAX_CONSECUTIVE_AUTH_FAILURES no rejection either, so an operator
        // who fat-fingers a pasted token is never locked out.
        let ok = get_root(&s, Some("right-token"));
        assert!(ok.contains("HTTP/1.1 200"), "{ok}");
        assert_eq!(s.auth_failure_count_for_test(LOCALHOST), 0, "success resets the streak");

        // The next wrong guess starts a fresh streak at 1, not 6.
        let r = get_root(&s, Some("guess"));
        assert!(r.contains("HTTP/1.1 401"), "{r}");
        assert_eq!(s.auth_failure_count_for_test(LOCALHOST), 1);
    }

    /// [A1] Behind a trusted proxy every request shares ONE source address,
    /// so a per-IP lockout would let one attacker's bad guesses refuse every
    /// user behind it — a self-inflicted outage in the deployment we
    /// recommend. A proxy-proven request is exempt; throttling there belongs
    /// to the proxy, like TLS and the IdP handshake.
    #[test]
    fn a_proxy_proven_request_is_never_locked_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = Areev::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        let facade = AreevFacade::with_session(store, Some("caller".into()), None);
        let s = UiServer::new(facade, "proxy-test".into())
            .with_auth("right-token".into())
            .with_sso_rotating("x-forwarded-user", "proxy-secret", None::<String>);

        // Burn well past the threshold with wrong bearer tokens, but all
        // arriving through the proxy (valid proxy secret on every request).
        for _ in 0..(super::MAX_CONSECUTIVE_AUTH_FAILURES + 5) {
            let _ = get_root_with(
                &s,
                "Authorization: Bearer guess\r\nX-Areev-Proxy-Secret: proxy-secret\r\n",
            );
        }
        // A legitimate user behind the same proxy is still served.
        let ok = get_root_with(
            &s,
            "Authorization: Bearer right-token\r\nX-Areev-Proxy-Secret: proxy-secret\r\n",
        );
        assert!(ok.contains("HTTP/1.1 200"), "proxy-borne traffic must not be locked out: {ok}");
    }

    /// [A1] Past the threshold the console stops answering credential-bearing
    /// requests from that source — refusing, never sleeping, because a serial
    /// accept loop makes a sleep a denial-of-service lever for everyone else.
    #[test]
    fn a_long_streak_is_refused_with_429_until_it_goes_idle() {
        let s = server();
        for i in 0..super::MAX_CONSECUTIVE_AUTH_FAILURES {
            let r = get_root(&s, Some("guess"));
            assert!(r.contains("HTTP/1.1 401"), "attempt {i} should still be a 401: {r}");
        }

        // The next credential-bearing request is refused before routing.
        let blocked = get_root(&s, Some("guess"));
        assert!(blocked.contains("HTTP/1.1 429"), "{blocked}");
        assert!(blocked.contains("Retry-After:"), "a 429 must say when: {blocked}");

        // The brake is on the SOURCE, not on the credential: even the right
        // token is refused while the streak stands. That is the intended
        // trade — an attacker who has burned an IP cannot then race a guess
        // through — and it is why the streak expires on its own.
        let right = get_root(&s, Some("right-token"));
        assert!(right.contains("HTTP/1.1 429"), "{right}");

        // A credential-LESS request is untouched: it was never counted, so it
        // must not be blocked either. The console page still serves its 401
        // challenge, which is what lets a browser prompt for a fresh login.
        let bare = get_root(&s, None);
        assert!(bare.contains("HTTP/1.1 401"), "{bare}");
    }
}
