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
    /// Shared secret required for access when set. In hub mode it guards only
    /// mutating/sync endpoints (`Bearer`); in console-auth mode (`auth_all`) it
    /// guards every request via `Bearer` **or** HTTP `Basic` (password = token).
    token: Option<String>,
    /// When true, *every* request requires the token — the console page, all
    /// reads, and all writes — and a `401` carries `WWW-Authenticate: Basic` so
    /// browsers prompt. Set by [`UiServer::with_auth`]; hub mode leaves it off.
    auth_all: bool,
    /// Directory for received segments (areevd hub mode).
    segment_dir: Option<String>,
    /// When false (the default), reject any request whose `Host` header is not
    /// loopback — the standard DNS-rebinding defense for a localhost service.
    /// Set true only when the operator intentionally serves to other hosts
    /// (CLI `--allow-remote`), where a non-loopback `Host` is expected.
    allow_remote: bool,
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
    sso_secret: Option<String>,
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
        UiServer {
            facade: std::sync::Arc::new(facade),
            executor: CalExecutor::new(CalExecutorConfig::default())
                .with_governance(std::sync::Arc::new(areev_loop_adapter::LoopGovernance::new())),
            db_label,
            token: None,
            auth_all: false,
            segment_dir: None,
            allow_remote: false,
            loop_policy: None,
            credentials: None,
            allow_destructive_ops: CalExecutorConfig::default().allow_destructive_ops,
            #[cfg(feature = "tls")]
            tls_config: None,
            sso_header: None,
            sso_secret: None,
        }
    }

    /// Enable trusted-header SSO (v0). `header` names the identity header
    /// the authenticating proxy forwards; `secret` is the proxy's shared
    /// secret, demanded in `x-areev-proxy-secret` on every SSO request.
    pub fn with_sso(mut self, header: impl Into<String>, secret: impl Into<String>) -> Self {
        let header = header.into();
        let secret = secret.into();
        // An empty proxy secret would let `x-areev-proxy-secret:` (empty)
        // prove any identity — refuse loudly at the library boundary too,
        // not just in the CLI.
        assert!(
            !header.trim().is_empty() && !secret.trim().is_empty(),
            "with_sso requires a non-empty header name and proxy secret"
        );
        self.sso_header = Some(header.to_ascii_lowercase());
        self.sso_secret = Some(secret);
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

    /// May this request's session read the raw sync surface? A segment is a
    /// whole-memory op-log export, so it takes `read` on every namespace —
    /// a principal granted read on one namespace must not be able to pull
    /// the file wholesale through `/api/segment`. Owner sessions (every
    /// non-credential-map deployment) pass unchanged.
    fn session_may_sync(&self) -> bool {
        self.facade
            .authz()
            .allows(areev_core::authz::Verb::Read, "*")
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

    /// areevd mode: require `Authorization: Bearer <token>` on POST
    /// endpoints and accept segment push/pull under `dir`.
    pub fn into_hub(mut self, token: Option<String>, dir: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        self.token = token;
        self.segment_dir = Some(dir.to_string());
        // The hub is a network service (segment push/pull from other nodes), so
        // non-loopback Hosts are expected — writes stay bearer-gated.
        self.allow_remote = true;
        Ok(self)
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
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(cert_pem)?))
                .collect::<Result<_, _>>()?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(std::fs::File::open(key_pem)?))?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no private key in PEM"))?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        self.tls_config = Some(std::sync::Arc::new(config));
        Ok(self)
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
        // to cross-site requests. Only loopback pages (the console itself)
        // may mutate; curl/CLI clients send no Origin and pass through.
        if method == "POST" && origin.as_deref().is_some_and(|o| !origin_is_local(o)) {
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

        // The identity header is TRUSTED only when the proxy proved itself
        // (constant-time secret check). A forged header without the secret
        // is silently ignored — the request proceeds as whatever its other
        // credentials make it.
        let sso_identity: Option<String> = match (&self.sso_secret, proxy_secret, sso_identity_raw) {
            (Some(expected), Some(presented), Some(id))
                if ct_eq(expected.as_bytes(), presented.as_bytes())
                    && !id.trim().is_empty() =>
            {
                Some(id)
            }
            _ => None,
        };
        let (status, ctype, payload) =
            self.route(&method, &path, &body, bearer.as_deref(), sso_identity.as_deref());
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

    fn route(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        bearer: Option<&str>,
        sso_identity: Option<&str>,
    ) -> (&'static str, &'static str, Vec<u8>) {
        // Auth: console-auth mode (`auth_all`) guards every request; hub mode
        // guards only mutating + sync endpoints. The credential arrives as a
        // Bearer token or an HTTP Basic password (see `handle_request`).
        //
        // A credential map changes WHO a request is, not WHETHER the shared
        // secret is required. When both are configured, a map token
        // authenticates as its principal, the shared secret authenticates as
        // the implied admin (`docs/cal-all-you-need-proposal.md` §5.2b), and
        // anything else is refused — an attached map must never silently
        // downgrade a `--token-env` console to an open `anonymous` one.
        let mut shared_secret_ok = false;
        if let Some(tok) = &self.token {
            let guarded = self.auth_all || method == "POST" || path.starts_with("/api/segment");
            shared_secret_ok = bearer.is_some_and(|b| ct_eq(b.as_bytes(), tok.as_bytes()));
            let known = shared_secret_ok
                || sso_identity.is_some()
                || match (&self.credentials, bearer) {
                    (Some(map), Some(b)) => map.resolve_for_memory(b, &self.db_label).is_ok(),
                    _ => false,
                };
            if guarded && !known {
                return ("401 Unauthorized", "application/json",
                        br#"{"ok":false,"error":"authentication required"}"#.to_vec());
            }
        } else if self.credentials.is_none() && sso_identity.is_none() && method == "POST" {
            // §5.7: token-less `areev ui` is read-only. The ONLY POST allowed is
            // a read-only CAL statement; every write (any loop mutation, an
            // ADD/SUPERSEDE/FORGET CAL batch, etc.) requires --token-env. This
            // closes the bypass where a local process could execute a
            // proposal's CAL directly and skip the review queue.
            let base = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
            let allowed = base == "/api/cal" && cal_body_is_read_only(body);
            if !allowed {
                return ("401 Unauthorized", "application/json",
                        br#"{"ok":false,"error":"read-only console: restart areev ui with --token-env VAR to enable writes"}"#.to_vec());
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
        let _sso_session = match (&request_principal, sso_identity, shared_secret_ok) {
            (None, Some(identity), false) => {
                request_principal = Some(identity.to_string());
                Some(RequestBinding::bind_identity(&self.facade, identity))
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
                    .replace("{{DB}}", &html_escape(&self.db_label))
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
                        "db": self.db_label,
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
                ok_json(json!({
                    "ok": true,
                    "db": self.db_label,
                    "warnings": warnings,
                    "file": {
                        "text_index": index_text,
                        "embedding": declared_embed.map(|(m, d)| json!({"model": m, "dim": d})),
                    },
                    "session": {
                        "namespace": self.facade.session_namespace(),
                        "mounts": self.facade.mount_aliases(),
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
                        "hub_mode": self.segment_dir.is_some(),
                        "auth_required": self.token.is_some(),
                        // true = every request is authenticated (console-auth);
                        // false with auth_required = hub mode (writes/sync only).
                        "auth_all": self.auth_all,
                        "segment_dir": self.segment_dir,
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
                // Scope the browse to the session namespace, the way CAL
                // already scopes recall. Without this the rail lists entities
                // (`john`, `bob`) that every query on this console then reports
                // as missing, because the two disagree about what is in view.
                // Rows we cannot attribute — tombstone stubs, grains erased
                // since — are kept: dropping them would hide an erasure.
                let built = built.map(|(total, rows)| {
                    let Some(ns) = self.facade.session_namespace() else {
                        return (total, rows);
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
                    (total, kept)
                });
                match built {
                    Ok((total, rows)) => ok_json(json!({"ok": true, "total_ops": total, "grains": rows})),
                    Err(e) => ok_json(json!({"ok": false, "error": e})),
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
            ("GET", "/api/verify") => match self.facade.with_store(|m| m.verify()) {
                Ok(r) => ok_json(json!({
                    "integrity": r.integrity, "grains": r.grains,
                    "hash_mismatches": r.hash_mismatches, "undecodable": r.undecodable,
                })),
                Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
            },
            ("POST", "/api/segment") => {
                // A pushed segment is imported straight into the live store,
                // so it is a write — and one that never crosses the facade's
                // verb checks. Gate it on the bound session's `write`, or a
                // principal with no write grant could add grains here that
                // `POST /api/cal` refuses it.
                if let Err(e) = self
                    .facade
                    .authz()
                    .check(areev_core::authz::Verb::Write, "*")
                {
                    return ("403 Forbidden", "application/json",
                            json!({"ok": false, "error": e.to_string()}).to_string().into_bytes());
                }
                // push: body = raw MGB1 bundle; applied immediately + archived
                let Some(dir) = &self.segment_dir else {
                    return ("400 Bad Request", "application/json",
                            br#"{"ok":false,"error":"not in hub mode"}"#.to_vec());
                };
                let name = q("name").unwrap_or_else(|| format!("push-{}.mgb", now_label()));
                let Some(safe) = safe_segment_name(&name) else {
                    return ("400 Bad Request", "application/json",
                            br#"{"ok":false,"error":"invalid segment name"}"#.to_vec());
                };
                let path = format!("{dir}/{safe}");
                if std::fs::write(&path, body).is_err() {
                    return ("500 Internal Server Error", "application/json",
                            br#"{"ok":false,"error":"write failed"}"#.to_vec());
                }
                match self.facade.with_store(|m| m.import_bundle(&path)) {
                    Ok(st) => ok_json(json!({"ok": true, "applied": st.applied, "skipped": st.skipped, "stored": safe})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/segments") | ("GET", "/api/segment") if !self.session_may_sync() => (
                "403 Forbidden",
                "application/json",
                br#"{"ok":false,"error":"AUT-E001: this session lacks read on the whole memory"}"#.to_vec(),
            ),
            ("GET", "/api/segments") => {
                let Some(dir) = &self.segment_dir else {
                    return ok_json(json!([]));
                };
                let mut names: Vec<String> = std::fs::read_dir(dir)
                    .map(|d| d.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                        .filter(|n| n.ends_with(".mgb")).collect())
                    .unwrap_or_default();
                names.sort();
                ok_json(json!(names))
            }
            ("GET", "/api/segment") => {
                let Some(dir) = &self.segment_dir else {
                    return ("400 Bad Request", "application/json", b"{}".to_vec());
                };
                let name = q("name").unwrap_or_default();
                let Some(safe) = safe_segment_name(&name) else {
                    return ("400 Bad Request", "application/json",
                            br#"{"ok":false,"error":"invalid segment name"}"#.to_vec());
                };
                match std::fs::read(format!("{dir}/{safe}")) {
                    Ok(b) => ("200 OK", "application/octet-stream", b),
                    Err(_) => ("404 Not Found", "application/json",
                               br#"{"ok":false,"error":"no such segment"}"#.to_vec()),
                }
            }
            // ── Areev Loop API (§5.4) — GETs are reads (token-less OK); the POST
            //    mutations are guarded above (token-less → 401). ────────────
            // ── /api/run/*: the governed runtime's console surface ──
            ("GET", "/api/run/list") => {
                let runner = self.runner("console:read");
                match runner.recent_runs(50) {
                    Ok(ids) => {
                        let rows: Vec<Value> = ids
                            .iter()
                            .map(|id| {
                                let outcome = self.facade.with_store(|m| {
                                    m.recent(
                                        "agent:harness",
                                        Some(areev_core::types::GrainType::Observation),
                                        4096,
                                    )
                                })
                                .ok()
                                .and_then(|rows| {
                                    rows.into_iter().find(|g| {
                                        g.get_str("run_id") == Some(id.as_str())
                                            && g.get_str("observation_kind")
                                                == Some("run_outcome")
                                    })
                                });
                                serde_json::json!({
                                    "run_id": id,
                                    "outcome": outcome.as_ref().and_then(|g| g.get_str("object")).unwrap_or("open"),
                                })
                            })
                            .collect();
                        ok_json(serde_json::json!({"ok": true, "runs": rows}))
                    }
                    Err(e) => run_api_err(&e.to_string()),
                }
            }
            ("GET", "/api/run/inspect") => {
                let Some(run_id) = q("run_id") else {
                    return run_api_err("run_id required");
                };
                let manifest = self
                    .facade
                    .with_store(|m| areev_run::RunManifest::load(m, &run_id));
                let manifest = match manifest {
                    Ok(m) => m,
                    Err(e) => return run_api_err(&e.to_string()),
                };
                let ns = self.facade.default_namespace().unwrap_or("shared").to_string();
                let view = self
                    .facade
                    .with_store(|m| areev_run::journal::load(m, &ns, &run_id));
                let (checkpoints, entries, scheduler) = match view {
                    Ok(v) => {
                        let sched = v
                            .checkpoints
                            .last()
                            .map(|c| c.scheduler.clone())
                            .unwrap_or(Value::Null);
                        (v.checkpoints.len(), v.entries.len(), sched)
                    }
                    Err(_) => (0, 0, Value::Null),
                };
                ok_json(serde_json::json!({
                    "ok": true,
                    "run_id": run_id,
                    "plan_hash": manifest.plan_hash,
                    "principal": manifest.principal,
                    "checkpoints": checkpoints,
                    "journal_entries": entries,
                    "spent": scheduler.get("spent"),
                    "pending_asks": scheduler.get("pending_asks"),
                }))
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
        match self.engine().apply(
            &mut sub, hash, actor, observer, &scopes, because, allow_destructive, now_ms(),
        ) {
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

fn now_label() -> String {
    format!("{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0))
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
/// Hand-rolled to keep the server dependency-free. Returns `None` on any
/// invalid character or truncated input.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Extract the password from an HTTP Basic credential (`base64("user:pass")`).
/// The console ignores the username and treats the password as the token.
/// Returns `None` if the value is not valid base64/UTF-8 or has no `:`.
fn basic_auth_password(b64: &str) -> Option<String> {
    let text = String::from_utf8(base64_decode(b64)?).ok()?;
    text.split_once(':').map(|(_user, pass)| pass.to_string())
}

/// Sanitize a client-supplied segment name into a single safe filename.
/// Strips anything outside `[A-Za-z0-9-._]`, then requires the result to be
/// exactly one normal path component — rejecting empty names, `.`/`..`, and
/// path separators so a push/pull cannot escape the segment directory.
fn safe_segment_name(name: &str) -> Option<String> {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect();
    if safe.is_empty() || safe.len() > 128 {
        return None;
    }
    let mut comps = std::path::Path::new(&safe).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(c)), None) if c.to_str() == Some(safe.as_str()) => {
            Some(safe)
        }
        _ => None,
    }
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
    use super::{base64_decode, basic_auth_password, ct_eq, safe_segment_name};

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

    #[test]
    fn safe_segment_name_blocks_traversal() {
        assert_eq!(safe_segment_name("push-123.mgb").as_deref(), Some("push-123.mgb"));
        assert_eq!(safe_segment_name(".."), None);
        assert_eq!(safe_segment_name("."), None);
        assert_eq!(safe_segment_name(""), None);
        // Separators are stripped, so any accepted name is a single component
        // that cannot escape the segment directory.
        for probe in ["../../etc/passwd", "/etc/passwd", "a/../b", "foo/bar"] {
            if let Some(s) = safe_segment_name(probe) {
                assert!(!s.contains('/') && s != ".." && s != ".", "unsafe: {s}");
            }
        }
    }
}

#[cfg(test)]
mod credential_route_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

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
}

#[cfg(test)]
mod memory_scoped_token_tests {
    use super::UiServer;
    use areev_cal::AreevFacade;
    use areev_core::authz::{CredentialMap, AUTHZ_NS, REL_PERMITS};
    use areev_core::types::{Fact, Grain};
    use areev_store::Areev;

    /// The enterprise-plane hub rule: one auth file, per-memory token
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
        for c in rustls_pemfile::certs(&mut std::io::BufReader::new(
            std::fs::File::open(&cert).unwrap(),
        )) {
            roots.add(c.unwrap()).unwrap();
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
        UiServer::new(facade, "sso-test".into()).with_sso("x-forwarded-user", "proxy-secret")
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
}
