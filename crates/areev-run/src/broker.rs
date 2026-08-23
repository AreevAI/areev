//! The credential broker — a loopback service that makes outbound calls on a
//! connector's behalf so the connector never holds the token.
//!
//! Posta's CB4A calls this Model A: *"The safest credential for an AI agent is
//! one it never holds."* The connector calls us unauthenticated over loopback,
//! we check the destination against its allowlist, attach credentials, and make
//! the real request. Cloudflare shipped exactly this in April 2026 ("no token
//! is ever passed into the sandbox"); Deno in February; it is the whole of
//! Nango's product.
//!
//! ## The shape, and its honest cost
//!
//! This is a **reverse** broker, not a forward proxy, and the difference
//! matters. A forward proxy cannot inject an `Authorization` header into an
//! HTTPS request without terminating TLS, which means shipping a CA the
//! connector trusts — a much larger and more dangerous mechanism. So instead the
//! connector posts us a *description* of the call it wants:
//!
//! ```json
//! { "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
//!   "method": "GET", "credential": "gmail",
//!   "headers": { "X-Goog-User-Project": "my-project" } }
//! ```
//!
//! and we answer with the response. The cost is real and worth stating: a
//! connector written this way cannot use a vendor SDK, because the SDK wants to
//! make its own sockets. That is the same trade Nango makes.
//!
//! ## What the connector gets instead of a secret
//!
//! `AREEV_EGRESS_URL` in its environment. Nothing else. The credential values
//! stay in this process, read from host-named environment variables, and never
//! appear in a grain — a declaration names a credential, it never carries one.
//!
//! ## Bind and reach
//!
//! Loopback only, on an ephemeral port, for the lifetime of one evaluation
//! pass. It is not a server anyone deploys, it has no configuration file, and
//! it does not outlive the command — consistent with a product whose stance is
//! that nothing stays resident.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use areev_core::types::capability::Declaration;

use crate::egress::{EgressDenied, EgressPolicy};

/// Config errors are plain messages; each host wraps them in its own error.
type Result<T> = std::result::Result<T, String>;

/// Largest request body the broker will read, mirroring the console's cap.
const MAX_BODY: usize = 1024 * 1024;

/// How a credential is attached to an outbound request.
#[derive(Debug, Clone)]
pub enum Credential {
    /// `Authorization: Bearer <value>`
    Bearer(String),
    /// A named header carrying the value verbatim.
    Header { name: String, value: String },
}

impl Credential {
    /// Resolve from an environment variable **name**, never a literal.
    ///
    /// The host names the variable, the same discipline `--passphrase-env` and
    /// `--token-env` use: a value that appears on a command line is a value in
    /// shell history and in `ps` output.
    ///
    /// ## Reading the name is also what registers it as a secret
    ///
    /// Withholding happens HERE rather than at each host's flag-parsing site,
    /// and that placement is the fix for #100. Until 1.6.0 the withhold list
    /// was three flags long (`--passphrase-env`, `--token-env`,
    /// `--anon-key-env`, registered in the CLI's one choke point) and
    /// `--credential NAME=VAR` was not on it — so the raw credential stayed in
    /// the inherited environment of every tool, connector and sandbox
    /// subprocess. A tool could read the secret straight out of its own
    /// environment and never call the broker at all, which is the precise
    /// opposite of what brokering is for.
    ///
    /// Four hosts read credentials this way — `areev run`'s `--credential`,
    /// `areev trigger run`'s, and the Python and Node bindings — so a fix at
    /// any one call site would have left the other three open. Registering as
    /// a side effect of *reading the name* is what makes it structural: a
    /// credential this process can resolve is a credential its children cannot
    /// see. The child still receives `AREEV_EGRESS_URL` + `AREEV_EGRESS_TOKEN`,
    /// which are applied AFTER the environment policy (`proc::run`), so the
    /// broker handshake is unaffected.
    pub fn bearer_from_env(var: &str) -> Result<Credential> {
        let v = std::env::var(var)
            .map_err(|_| format!("credential env var {var} is not set"))?;
        if v.trim().is_empty() {
            return Err(format!("credential env var {var} is empty"));
        }
        areev_core::proc::deny_env_var(var);
        Ok(Credential::Bearer(v))
    }

    /// Parse `ENV_VAR` or `ENV_VAR@principal` — the spec form `--credential
    /// NAME=…` takes — returning the credential and its owner, if bound.
    ///
    /// The owner is the **run principal this credential belongs to**. A bound
    /// credential is refused for any run executing as anyone else — including
    /// a run with no principal bound at all, which fails closed (see
    /// [`Broker::bind_credential_owner`]). `@` is safe as the separator
    /// because an environment variable name cannot contain one, and a
    /// principal (`user:alice`) can contain `:`, which rules the natural
    /// alternative out.
    pub fn bearer_from_env_spec(spec: &str) -> Result<(Credential, Option<String>)> {
        let (var, owner) = match spec.split_once('@') {
            Some((v, o)) if !o.trim().is_empty() => (v.trim(), Some(o.trim().to_string())),
            // `VAR@` with an empty principal is a typo, or an unset shell
            // variable expanded into the owner position (`VAR@$OWNER` with
            // $OWNER unset) — NOT a request for an unbound credential. Treating
            // it as unbound fails OPEN: the confinement the operator spelled
            // out silently disappears. Refuse it instead; an unbound credential
            // is spelled `NAME=VAR`, with no `@` at all.
            Some((_, _)) => {
                return Err(format!(
                    "credential spec {spec:?} has an empty principal after '@' — write \
                     NAME=VAR for an unbound credential, or NAME=VAR@principal to bind one"
                ))
            }
            None => (spec.trim(), None),
        };
        Ok((Self::bearer_from_env(var)?, owner))
    }
}

/// What one caller may do through the broker.
///
/// Deny by default in both directions: a caller with no grant may do nothing,
/// and a grant that names no methods may only read. Connectors read; tools
/// write, and the write verb is exactly the one worth making deliberate.
#[derive(Debug, Clone, Default)]
pub struct CallerGrant {
    /// Credential names this caller may ask for. Empty = none.
    pub credentials: std::collections::BTreeSet<String>,
    /// Methods it may issue. Empty = `GET`/`HEAD` only.
    pub methods: std::collections::BTreeSet<String>,
}

impl CallerGrant {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn credential(mut self, name: &str) -> Self {
        self.credentials.insert(name.to_string());
        self
    }
    pub fn method(mut self, m: &str) -> Self {
        self.methods.insert(m.trim().to_ascii_uppercase());
        self
    }
    fn permits_method(&self, method: &str) -> bool {
        if self.methods.is_empty() {
            return matches!(method, "GET" | "HEAD");
        }
        self.methods.contains(method)
    }
}

/// Who may do what, keyed by caller name (a tool name, or a connector name).
///
/// ## Why the grant is host config and not a grain
///
/// A `Tool` Definition declaring "I may reach api.example.com with credential
/// X" would be a permission arriving in the same bundle as the code it
/// authorizes — the exact thing [`crate::executor::CodeExecutor`] refuses for
/// code. The tool says which credential it wants *at call time*, by name; the
/// host decides whether it may have it. Intent travels; authority does not.
#[derive(Debug, Clone, Default)]
pub struct EgressGrants {
    by_caller: BTreeMap<String, CallerGrant>,
    /// Applied to a caller with no entry of its own. `None` = such a caller
    /// gets nothing. The connector path sets this, because one connector runs
    /// per pass and there is no second caller to tell it apart from.
    default_grant: Option<CallerGrant>,
}

impl EgressGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant `caller` (a tool name) a specific scope.
    pub fn grant(mut self, caller: &str, g: CallerGrant) -> Self {
        self.by_caller.insert(caller.to_string(), g);
        self
    }

    /// Give every unlisted caller this scope. Spelled out so it cannot be
    /// reached by writing `default()`.
    pub fn default_for_all(mut self, g: CallerGrant) -> Self {
        self.default_grant = Some(g);
        self
    }

    fn for_caller(&self, caller: &str) -> Option<&CallerGrant> {
        self.by_caller.get(caller).or(self.default_grant.as_ref())
    }

    fn callers(&self) -> Vec<String> {
        self.by_caller.keys().cloned().collect()
    }
}

/// An unguessable per-caller capability token.
///
/// The broker binds to loopback, and loopback is not an authorization: any
/// process on the box could otherwise post to it and spend the credentials it
/// holds. The token also makes per-caller scoping possible at all — without
/// it the broker cannot tell which tool is calling, and N pool workers share
/// one port.
fn mint_token() -> String {
    let mut b = [0u8; 24];
    // A broker that cannot get randomness must not fall back to something
    // guessable; the caller turns this into a refusal to start.
    getrandom::getrandom(&mut b).expect("OS randomness for the egress token");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One refused outbound call, kept for the audit record.
///
/// A refusal is an agent reaching for somewhere it was not allowed — the
/// single most audit-worthy event this subsystem produces, and the one a
/// reviewer asks about ("did it ever try?"). stderr answers that only until
/// the terminal scrolls, so the driver journals these into the memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRefusal {
    /// The tool or connector that asked. Empty for a caller with no name of
    /// its own (the connector path's default grant).
    pub caller: String,
    /// Where it tried to go, as the caller spelled it.
    pub destination: String,
    /// Why it was refused, in one phrase.
    pub reason: String,
}

/// One call that WENT OUT, kept for the audit trail (#101).
///
/// A capability tool's whole bargain is that its I/O is mediated and
/// recorded, so the successful calls matter as much as the refused ones: "it
/// was allowed to reach Gmail" is a policy statement, "it sent these four
/// requests" is the evidence. Bodies are recorded as **digests**, never
/// contents — the journal is an immutable, replicating grain and a mailbox
/// body does not belong in one.
///
/// Deliberately NOT a journal entry, for the same reason [`EgressRefusal`] is
/// not: replay never sees it, so `verify` stays byte-identical whether or not
/// a broker was configured. It is evidence about the run, not a step of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressCall {
    /// The tool that asked.
    pub caller: String,
    pub method: String,
    /// The URL as dispatched. When a redirect was followed this is the FINAL
    /// URL, and `redirects` says how many hops it took to get there.
    pub url: String,
    pub status: u16,
    pub redirects: u32,
    /// `sha256:<hex>` of the request body, or `None` when there was none.
    pub request_digest: Option<String>,
    /// `sha256:<hex>` of the response body.
    pub response_digest: String,
    pub response_bytes: usize,
    /// The credential NAME that was attached, never a value.
    pub credential: Option<String>,
    /// Non-credential request headers the caller set (#105), name AND value.
    ///
    /// Recorded in full, unlike the credential and unlike bodies: the caller
    /// supplied these, so they carry nothing the caller did not already know,
    /// and "it sent these four requests with these headers" is strictly more
    /// evidence than "it sent these four requests".
    pub headers: BTreeMap<String, String>,
}

/// Per-caller ceilings on a capability tool's mediated egress.
///
/// Extism's model: overruns are typed errors, never truncation — a tool that
/// silently received half a response would produce a wrong answer with no
/// evidence that anything went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityLimits {
    /// Calls one caller may make for the life of the broker.
    pub max_calls: u32,
    /// Largest response body handed back to a caller.
    pub max_response_bytes: usize,
}

impl Default for CapabilityLimits {
    fn default() -> Self {
        CapabilityLimits { max_calls: 64, max_response_bytes: 1024 * 1024 }
    }
}

/// What a caller declared, and what it may spend.
#[derive(Debug, Clone, Default)]
struct Declared {
    declaration: areev_core::types::capability::Declaration,
    limits: CapabilityLimits,
    calls_made: u32,
}

/// A running broker. Dropping it stops the listener.
pub struct Broker {
    url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Destinations refused during this pass, for journalling.
    refusals: Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
    /// Calls that succeeded, for journalling (#101).
    calls: Arc<std::sync::Mutex<Vec<EgressCall>>>,
    /// caller -> what its Definition declared. A caller with no entry is a
    /// subprocess tool or connector, which declares nothing and is governed
    /// by the host grant alone — today's behaviour, unchanged.
    declared: Arc<std::sync::Mutex<BTreeMap<String, Declared>>>,
    /// credential name -> the run principal it belongs to. A credential with
    /// no entry is spendable by any run the grant admits — today's behaviour.
    credential_owners: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    /// The principal the CURRENT run executes as, bound by the driver at
    /// drive entry. `None` until a run binds one — and an owned credential
    /// refuses under `None`, so a path that never binds (the trigger
    /// evaluator's connector pass, a bare library embedding) fails closed
    /// rather than open.
    run_principal: Arc<std::sync::Mutex<Option<String>>>,
    /// caller -> its capability token.
    tokens: BTreeMap<String, String>,
    /// The token an unlisted caller presents, when a default grant exists.
    default_token: Option<String>,
}

impl Broker {
    /// Start a broker on an ephemeral loopback port.
    pub fn start(
        policy: EgressPolicy,
        credentials: BTreeMap<String, Credential>,
        grants: EgressGrants,
        refusal_code: &'static str,
    ) -> Result<Broker> {
        // 127.0.0.1 explicitly, never 0.0.0.0: a broker that holds credentials
        // and is reachable off-box is a credential server.
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("egress broker cannot bind loopback: {e}"))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        listener.set_nonblocking(true).ok();

        let stop = Arc::new(AtomicBool::new(false));
        let refusals = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let declared: Arc<std::sync::Mutex<BTreeMap<String, Declared>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let credential_owners: Arc<std::sync::Mutex<BTreeMap<String, String>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let run_principal: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let (stop_t, refusals_t) = (Arc::clone(&stop), Arc::clone(&refusals));
        let (calls_t, declared_t) = (Arc::clone(&calls), Arc::clone(&declared));
        let (owners_t, principal_t) = (Arc::clone(&credential_owners), Arc::clone(&run_principal));

        // One token per caller, minted before the listener serves anything.
        let mut tokens = BTreeMap::new();
        for c in grants.callers() {
            tokens.insert(c, mint_token());
        }
        let default_token = grants.default_grant.as_ref().map(|_| mint_token());
        let by_token: BTreeMap<String, String> = tokens
            .iter()
            .map(|(c, t)| (t.clone(), c.clone()))
            .chain(default_token.iter().map(|t| (t.clone(), String::new())))
            .collect();

        let handle = std::thread::spawn(move || {
            while !stop_t.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // On macOS/BSD an accepted socket INHERITS O_NONBLOCK
                        // from its listener, so the first read returns
                        // WouldBlock and the request is never seen. The
                        // listener is non-blocking so the accept loop can
                        // notice the stop flag; the connection must not be.
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        // Served one at a time, matching the console's
                        // one-request-per-connection posture: this brokers for
                        // a single connector subprocess, not for a fleet.
                        let _ = serve_one(
                            stream,
                            &policy,
                            &credentials,
                            &refusals_t,
                            &calls_t,
                            &declared_t,
                            &owners_t,
                            &principal_t,
                            &grants,
                            &by_token,
                            refusal_code,
                        );
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Broker {
            url: format!("http://127.0.0.1:{port}"),
            stop,
            handle: Some(handle),
            refusals,
            calls,
            declared,
            credential_owners,
            run_principal,
            tokens,
            default_token,
        })
    }

    /// Bind `name` to the run principal that owns it (#101 follow-through).
    ///
    /// An owned credential is attached only for a run whose bound principal
    /// equals the owner — a run executing as anyone else, or a path that
    /// never bound a principal at all, is refused. This is what stops a
    /// process loaded with several principals' credentials from letting a
    /// run started on behalf of one spend another's: the grant says which
    /// TOOLS may ask, and this says which RUNS may be answered.
    pub fn bind_credential_owner(&self, name: &str, principal: &str) {
        if let Ok(mut owners) = self.credential_owners.lock() {
            owners.insert(name.to_string(), principal.to_string());
        }
    }

    /// Record the principal the current run executes as.
    ///
    /// Called by the driver at drive entry — every run, including resume and
    /// fork, passes through there — so the binding cannot be forgotten by a
    /// caller. One broker serves one evaluation pass at a time (its own
    /// documented lifetime), which is what makes a single slot sound; a host
    /// that interleaved principals through one broker would need one broker
    /// per principal, and gets fail-closed behaviour rather than
    /// mis-attribution if it forgets.
    pub fn bind_run_principal(&self, principal: &str) {
        if let Ok(mut p) = self.run_principal.lock() {
            *p = Some(principal.to_string());
        }
    }

    /// Register what `caller`'s Definition declared, so the broker can enforce
    /// `declared ∩ host-granted` on every call (#101).
    ///
    /// Idempotent per caller in the sense that re-registering the same
    /// declaration is harmless — but it deliberately does NOT reset the call
    /// counter, so a tool dispatched repeatedly cannot buy itself a fresh
    /// budget by being re-declared.
    ///
    /// A caller that never declares is unaffected: `--tool-cmd` tools and
    /// connectors keep exactly the host-grant-only behaviour they have today.
    pub fn declare(
        &self,
        caller: &str,
        declaration: areev_core::types::capability::Declaration,
        limits: CapabilityLimits,
    ) {
        if let Ok(mut map) = self.declared.lock() {
            let entry = map.entry(caller.to_string()).or_default();
            entry.declaration = declaration;
            entry.limits = limits;
        }
    }

    /// The capability token `caller` presents, if it has a grant.
    pub fn token_for(&self, caller: &str) -> Option<&str> {
        self.tokens
            .get(caller)
            .or(self.default_token.as_ref())
            .map(String::as_str)
    }

    /// Every call that actually went out, in order (#101).
    ///
    /// Read rather than drained, like [`Broker::refusals`]: the driver
    /// journals them and the CLI prints a summary. NOT deduplicated — a
    /// refusal is a policy fact and forty attempts are one of them, but a
    /// successful call is an *effect*, and forty of those are forty things
    /// that happened.
    pub fn calls(&self) -> Vec<EgressCall> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// What to put in the connector's `AREEV_EGRESS_URL`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every DISTINCT refusal so far.
    ///
    /// Deduplicated at the point of recording, on `(caller, destination,
    /// reason)`. A tool retrying against one blocked host is one audit fact,
    /// not forty — which is what bounds this list by the plan's shape rather
    /// than by how hard something retries. The count of attempts is not kept
    /// here; the operator-facing log line is per attempt.
    ///
    /// Read rather than drained, so several consumers can each see the whole
    /// set: the driver journals them, the CLI prints them when the run ends,
    /// and the trigger evaluator turns them into `TRG-E009`.
    pub fn refusals(&self) -> Vec<EgressRefusal> {
        self.refusals.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        // Signal and detach — deliberately NOT joined. The accept loop can be
        // inside `serve_one` waiting on an upstream that has up to 60s of
        // timeout left, and joining there would block the evaluator on a call
        // whose result nobody wants any more. The thread owns its listener and
        // exits at the next loop check, so the port is released promptly
        // without anyone waiting for it.
        self.stop.store(true, Ordering::Relaxed);
        drop(self.handle.take());
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct EgressRequest {
    url: String,
    method: String,
    /// Which credential to attach, by name. The connector chooses *which*, never
    /// *what* — it cannot name a value it was not given.
    credential: Option<String>,
    body: Option<String>,
    /// Non-credential request headers the caller wants set (#105).
    ///
    /// The enterprise APIs capability tools are pitched at need one:
    /// `X-Goog-User-Project` on every Google call made with user credentials,
    /// `anthropic-version`, `x-ms-version`, a tenant id. None of them is a
    /// secret — the caller supplies the value, so it is guest-visible by
    /// construction, which is why these may be journaled verbatim while a
    /// credential may only ever be journaled by name.
    ///
    /// A `BTreeMap` deliberately: one value per name, ordered, so the journal
    /// is deterministic and a caller cannot smuggle a second `X-Foo` past a
    /// check that looked at the first.
    headers: BTreeMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
/// Record a refusal once. Bounded by distinct refusals, not by attempts.
fn note_refusal(refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>, r: EgressRefusal) {
    if let Ok(mut list) = refusals.lock() {
        if !list.contains(&r) {
            list.push(r);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_one(
    mut stream: TcpStream,
    policy: &EgressPolicy,
    credentials: &BTreeMap<String, Credential>,
    refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
    calls: &Arc<std::sync::Mutex<Vec<EgressCall>>>,
    declared: &Arc<std::sync::Mutex<BTreeMap<String, Declared>>>,
    credential_owners: &Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    run_principal: &Arc<std::sync::Mutex<Option<String>>>,
    grants: &EgressGrants,
    by_token: &BTreeMap<String, String>,
    refusal_code: &'static str,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    let mut reader = BufReader::new(stream.try_clone()?);

    // Request line, then headers, then a Content-Length body.
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut content_length = 0usize;
    let mut presented: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        // Read off the ORIGINAL line: lowercasing the header name must not
        // lowercase the token's value.
        if lower.starts_with("x-areev-egress-token:") {
            presented = h.split_once(':').map(|(_, v)| v.trim().to_string());
        }
    }

    // Loopback is not an authorization: without this, any process on the box
    // could post here and spend the credentials the broker holds. It is also
    // what lets one port serve N pool workers and still tell them apart.
    let caller = match presented.and_then(|t| by_token.get(&t).cloned()) {
        Some(c) => c,
        None => {
            // The caller may still be mid-write on a body we are about to
            // refuse to read for its own sake; drain it so closing the
            // connection sends a clean FIN, not an RST that could clobber
            // their view of this very response.
            drain_body(&mut reader, content_length);
            return respond(
                &mut stream,
                401,
                &serde_json::json!({
                    "error": "missing or unknown X-Areev-Egress-Token",
                    "code": refusal_code
                })
                .to_string(),
            )
        }
    };
    let Some(grant) = grants.for_caller(&caller) else {
        drain_body(&mut reader, content_length);
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!("caller '{caller}' has no egress grant"),
                "code": refusal_code
            })
            .to_string(),
        );
    };
    if content_length > MAX_BODY {
        // Deliberately NOT drained: the whole point of this refusal is that
        // the caller claims a body large enough that reading it is the
        // resource-exhaustion risk. An abrupt reset here is the acceptable
        // side of that trade, unlike the two refusals above where the body is
        // always small and legitimate.
        return respond(&mut stream, 413, r#"{"error":"body too large"}"#);
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    let req: EgressRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return respond(
                &mut stream,
                400,
                &serde_json::json!({ "error": format!("bad egress request: {e}") }).to_string(),
            )
        }
    };

    // Guest headers, validated before anything else looks at them (#105).
    //
    // Two refusals with different characters. A malformed name or value is a
    // CLIENT error — the same shape as unparseable JSON — and a value carrying
    // CR/LF is header injection, which must die here rather than at the socket
    // where it would split one request into two.
    //
    // A broker-owned name is a policy refusal, and deliberately a FREE one:
    // the "probing is not free" rule that spends a call before the declaration
    // check exists because those answers differ per caller and so leak the
    // policy. "May I write the Authorization header?" has exactly one answer,
    // no, for every caller that ever asks — so answering it without charge
    // reveals nothing, and refusing early keeps the credential channel's
    // guarantee independent of budgets, declarations, and grants.
    for (name, value) in &req.headers {
        if !areev_core::types::capability::is_valid_header_name(name)
            || !areev_core::types::capability::is_valid_header_value(value)
        {
            return respond(
                &mut stream,
                400,
                &serde_json::json!({
                    "error": format!("header '{name}' is not a valid HTTP header name/value")
                })
                .to_string(),
            );
        }
        if areev_core::types::capability::is_broker_owned_header(name) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: req.url.clone(),
                    reason: format!("tried to set the broker-owned header '{name}'"),
                },
            );
            return respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!(
                        "caller '{caller}' tried to set header '{name}', which the broker owns — \
                         name a credential instead; the broker attaches it and the caller never \
                         holds one"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            );
        }
    }
    let guest_headers: Vec<(String, String)> =
        req.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let guest_header_names: Vec<String> = req.headers.keys().cloned().collect();

    if let Err(e) = policy.permits(&req.url) {
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.clone(),
                destination: req.url.clone(),
                reason: "destination outside the declared allowlist".into(),
            },
        );
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!("caller '{caller}' {e}"),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    let method = if req.method.trim().is_empty() { "GET" } else { req.method.trim() };
    let method = method.to_ascii_uppercase();

    // The DECLARED half of the intersection (#101), checked alongside the host
    // grant and never instead of it: a declaration can only narrow. A caller
    // with no declaration — every `--tool-cmd` tool and every connector — skips
    // this entirely and is governed by the host grant alone, as before.
    //
    // The call budget is spent here, before dispatch, so a refused call still
    // costs one: otherwise a module could probe the policy for free, and the
    // ceiling would bound successes rather than attempts. The declaration is
    // CLONED out of the registry so `dispatch` can re-apply it to every
    // redirect hop without holding the lock across network I/O — a hop is a
    // destination the caller never named, and it does not get to be the one
    // place the declared half goes unchecked.
    let capability: Option<(CapabilityLimits, Declaration)> = {
        let mut guard = declared.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&caller) {
            None => None,
            Some(d) => {
                if d.calls_made >= d.limits.max_calls {
                    let max = d.limits.max_calls;
                    drop(guard);
                    note_refusal(
                        refusals,
                        EgressRefusal {
                            caller: caller.clone(),
                            destination: req.url.clone(),
                            reason: format!("exceeded its ceiling of {max} brokered calls"),
                        },
                    );
                    return respond(
                        &mut stream,
                        403,
                        &serde_json::json!({
                            "error": format!(
                                "caller '{caller}' has made its {max} permitted brokered calls"
                            ),
                            "code": refusal_code
                        })
                        .to_string(),
                    );
                }
                d.calls_made += 1;
                if let Err(denied) = d.declaration.permits(
                    &req.url,
                    &method,
                    req.credential.as_deref(),
                    &guest_header_names,
                ) {
                    drop(guard);
                    note_refusal(
                        refusals,
                        EgressRefusal {
                            caller: caller.clone(),
                            destination: req.url.clone(),
                            reason: format!("undeclared capability: {denied}"),
                        },
                    );
                    return respond(
                        &mut stream,
                        403,
                        &serde_json::json!({
                            "error": format!("caller '{caller}' {denied}"),
                            "code": refusal_code
                        })
                        .to_string(),
                    );
                }
                Some((d.limits, d.declaration.clone()))
            }
        }
    };

    // For a capability caller, an unrestricted host policy does not extend to
    // private address space (#101). A memory that syncs in can declare any
    // hosts it likes, so the declaration alone must never be what authorizes
    // a request to the loopback console, the hub, a cloud metadata service,
    // or a LAN neighbour — reaching those takes an explicit `--allow-host`
    // entry, an operator's auditable act, exactly as executing the blob at
    // all takes the executor pin. Subprocess tools and connectors are
    // untouched: their reach was always pure host config.
    if capability.is_some() && policy.is_unrestricted() && crate::egress::is_private_destination(&req.url)
    {
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.clone(),
                destination: req.url.clone(),
                reason: "private or loopback destination without an explicit --allow-host entry"
                    .into(),
            },
        );
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!(
                    "caller '{caller}' tried to reach a private or loopback destination — a \
                     capability declaration alone cannot authorize one; the host must name it \
                     in --allow-host"
                ),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    // Connectors read; tools write. A grant that names no method may only
    // read, so the write verb is always something someone decided to allow.
    if !grant.permits_method(&method) {
        let denied = EgressDenied::Method { method: method.clone() };
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.clone(),
                destination: req.url.clone(),
                reason: format!("method {method} is not permitted for this caller"),
            },
        );
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!("caller '{caller}' {denied}"),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    // Resolve the credential to a header pair before dispatch. The connector
    // chooses WHICH credential by name; it can never name a value it was not
    // given, and no value ever crosses back to it.
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(name) = &req.credential {
        // Owned credentials bind to a RUN principal, not just to a tool. The
        // grant says which tools may ask; this says which runs may be
        // answered — a process loaded with several principals' credentials
        // must not let a run started on behalf of one spend another's. Fails
        // closed when no principal was bound at all: a path that never binds
        // (the trigger evaluator's connector pass, a bare embedding) gets a
        // refusal, never a quiet exception.
        // Recover a poisoned guard rather than fail OPEN on it: `.lock().ok()`
        // would yield `None`, skip the owner check entirely, and attach the
        // credential with no principal binding — disabling exactly the #101
        // isolation this block enforces. The `declared` map above already fails
        // closed this way (`into_inner`); the security-critical owner map must
        // too.
        let owner = {
            let owners = credential_owners.lock().unwrap_or_else(|e| e.into_inner());
            owners.get(name.as_str()).cloned()
        };
        if let Some(owner) = owner {
            let bound = run_principal.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if bound.as_deref() != Some(owner.as_str()) {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.clone(),
                        destination: req.url.clone(),
                        reason: format!(
                            "credential '{name}' is bound to principal '{owner}', and this run \
                             executes as {}",
                            bound.as_deref().unwrap_or("no bound principal")
                        ),
                    },
                );
                // The response names neither principal: the caller is a tool,
                // and whose credential this is happens to be none of its
                // business. The journaled refusal above carries both for the
                // operator.
                return respond(
                    &mut stream,
                    403,
                    &serde_json::json!({
                        "error": format!(
                            "caller '{caller}' may not use credential '{name}' — it is bound to \
                             a different run principal"
                        ),
                        "code": refusal_code
                    })
                    .to_string(),
                );
            }
        }
        // Scoped per caller: naming a credential is not the same as being
        // allowed to use it, which is the whole of the RBAC story here.
        if !grant.credentials.contains(name) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: req.url.clone(),
                    reason: format!("credential '{name}' is not granted to this caller"),
                },
            );
            return respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!("caller '{caller}' may not use credential '{name}'"),
                    "code": refusal_code
                })
                .to_string(),
            );
        }
        match credentials.get(name) {
            Some(Credential::Bearer(v)) => {
                headers.push(("Authorization".into(), format!("Bearer {v}")))
            }
            Some(Credential::Header { name, value }) => {
                headers.push((name.clone(), value.clone()))
            }
            None => {
                return respond(
                    &mut stream,
                    400,
                    &serde_json::json!({
                        "error": format!("no credential named '{name}' is configured for this run")
                    })
                    .to_string(),
                )
            }
        }
    }

    // The static list of broker-owned names is not the whole credential
    // channel: `Credential::Header` lets an operator carry a secret in ANY
    // header — `X-Api-Key`, `apikey`, whatever the upstream wants — and that
    // name is known only here, after resolution, because it is per-credential
    // host configuration rather than a constant. A guest header colliding with
    // it would be a guest writing a value into the exact slot the broker fills
    // with a secret. Refused for the same reason and with the same words
    // (#105).
    for (guest_name, _) in &guest_headers {
        let g = guest_name.trim().to_ascii_lowercase();
        if headers.iter().any(|(k, _)| k.trim().to_ascii_lowercase() == g) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: req.url.clone(),
                    reason: format!("tried to set the broker-owned header '{guest_name}'"),
                },
            );
            return respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!(
                        "caller '{caller}' tried to set header '{guest_name}', which carries a \
                         configured credential on this host — name the credential instead"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            );
        }
    }

    if !matches!(method.as_str(), "GET" | "HEAD" | "DELETE" | "POST" | "PUT" | "PATCH") {
        return respond(
            &mut stream,
            400,
            &serde_json::json!({ "error": format!("unsupported method '{method}'") }).to_string(),
        );
    }

    match dispatch(
        &req.url,
        &method,
        req.body.as_deref(),
        &headers,
        &guest_headers,
        policy,
        grant,
        capability.as_ref().map(|(_, d)| d),
        capability.as_ref().map(|(l, _)| l.max_response_bytes),
        &caller,
        refusals,
    ) {
        Dispatched::Answered { status, mut body, final_url, redirects, credential_sent } => {
            // Credential reflection: an echo or a verbose error endpoint can
            // bounce the injected `Authorization` back in its BODY, and that
            // body goes to the guest and (as a digest) into the audit trail.
            // Response HEADERS never cross this boundary at all — the broker
            // answers with `{status, body}` and nothing else — so the body is
            // the only channel, and it is scrubbed rather than trusted.
            for (_, value) in &headers {
                if value.len() >= 8 && body.contains(value.as_str()) {
                    body = body.replace(value.as_str(), "[redacted-credential]");
                }
                if let Some(bare) = value.strip_prefix("Bearer ") {
                    if bare.len() >= 8 && body.contains(bare) {
                        body = body.replace(bare, "[redacted-credential]");
                    }
                }
            }
            note_call(
                calls,
                EgressCall {
                    caller: caller.clone(),
                    method: method.clone(),
                    url: final_url,
                    status,
                    redirects,
                    request_digest: req.body.as_deref().map(digest),
                    response_digest: digest(&body),
                    response_bytes: body.len(),
                    // Only if the credential actually rode the FINAL request: a
                    // cross-origin redirect drops it (see `dispatch`), and an
                    // immutable audit grain claiming the secret reached a
                    // destination it never touched is a false record a DSAR or
                    // reviewer would read as fact.
                    credential: if credential_sent { req.credential.clone() } else { None },
                    // Recorded on the same "what actually rode the final
                    // request" rule as the credential, and for the same
                    // reason: guest headers travel exactly as far as the
                    // credential does (see `dispatch`), so a chain that left
                    // its origin sent neither.
                    headers: if credential_sent {
                        req.headers.clone()
                    } else {
                        BTreeMap::new()
                    },
                },
            );
            respond(
                &mut stream,
                200,
                &serde_json::json!({ "status": status, "body": body }).to_string(),
            )
        }
        Dispatched::Refused { detail } => respond(
            &mut stream,
            403,
            &serde_json::json!({ "error": detail, "code": refusal_code }).to_string(),
        ),
        // Overruns are typed errors, never truncation: a tool handed half a
        // response computes a wrong answer with nothing to show for it. The
        // read was abandoned at the cap, so the oversized body was never
        // buffered whole.
        Dispatched::TooLarge { final_url } => {
            let max =
                capability.as_ref().map(|(l, _)| l.max_response_bytes).unwrap_or_default();
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: final_url,
                    reason: format!("response exceeds its {max}-byte ceiling"),
                },
            );
            respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!(
                        "caller '{caller}' received a response larger than its {max}-byte \
                         ceiling — refused rather than truncated"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            )
        }
        Dispatched::Upstream(e) => respond(
            &mut stream,
            502,
            &serde_json::json!({ "error": format!("upstream: {e}") }).to_string(),
        ),
    }
}

/// `sha256:<hex>` over a body. The audit trail records what was sent and
/// received without recording a mailbox into an immutable, replicating grain.
fn digest(body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    format!("sha256:{:x}", h.finalize())
}

/// Record one successful call. Bounded so a runaway loop cannot exhaust
/// memory here; the ceiling is well above `CapabilityLimits::max_calls`, which
/// is the real bound for a capability tool.
fn note_call(calls: &Arc<std::sync::Mutex<Vec<EgressCall>>>, c: EgressCall) {
    const MAX_RECORDED_CALLS: usize = 4096;
    if let Ok(mut list) = calls.lock() {
        if list.len() < MAX_RECORDED_CALLS {
            list.push(c);
        }
    }
}

/// How many hops the broker follows before giving up. Matches ureq's own
/// default so the change in *who* follows a redirect does not also change how
/// many are tolerated.
const MAX_REDIRECT_HOPS: usize = 10;

/// The outcome of one brokered call, redirects included.
enum Dispatched {
    /// The upstream answered. Any status the caller is entitled to see,
    /// including a `3xx` this broker chose not to follow.
    ///
    /// `final_url` is where the response actually came from, which is not
    /// necessarily where the caller aimed — that distinction is the point of
    /// recording it.
    ///
    /// `credential_sent` is whether the credential actually rode the FINAL
    /// request: a cross-origin redirect drops it, so the audit must not claim a
    /// secret reached a destination it never did.
    ///
    /// It answers the same question for the caller's own headers (#105), which
    /// is not a coincidence to be maintained by hand: `perform` gates both on
    /// one boolean, so "the credential rode" and "the guest headers rode" are
    /// the same fact. If they ever stop being the same fact, this needs to
    /// become two flags — journaling a header that did not travel is the same
    /// false record as journaling a credential that did not.
    Answered { status: u16, body: String, final_url: String, redirects: u32, credential_sent: bool },
    /// A hop was refused by policy; already recorded in `refusals`.
    Refused { detail: String },
    /// The final response exceeded the caller's `max_response_bytes` — the
    /// read was ABANDONED at the cap, not completed and measured. A typed
    /// outcome rather than an oversized `Answered`, because the alternative on
    /// the error path was an empty string masquerading as the upstream's
    /// answer: a silent truncation to nothing, which is the exact failure mode
    /// the cap exists to make loud.
    TooLarge { final_url: String },
    /// The transport failed.
    Upstream(String),
}

/// Perform the call, following redirects **by hand** so the allowlist governs
/// every hop rather than only the first (#99).
///
/// ## Why the client is not allowed to do this for us
///
/// ureq follows up to ten redirects on its own. The broker checked
/// `policy.permits` once, against the URL the *caller* supplied, before
/// dispatch — so an allowed host answering `302 Location:
/// http://169.254.169.254/latest/meta-data/` sent the follow-up request to the
/// cloud metadata service and handed its body back to the tool. Host
/// allowlisting is this subsystem's core control, and a redirect walked
/// straight through it. Auto-follow is therefore off (`max_redirects(0)`) and
/// each hop is re-authorized here: **no byte is sent to, and no body is
/// returned from, a host the allowlist does not permit.**
///
/// ## And the mirror image, which was a silent breakage
///
/// ureq's `redirect_auth_headers` defaults to `Never`, so it dropped the
/// brokered `Authorization` on *every* redirect — including a same-origin one,
/// which Google and Microsoft APIs use routinely. The follow-up arrived
/// unauthenticated, 401'd, and nothing in the journal said why. Here the
/// credential is re-attached exactly when [`crate::egress::same_origin`] holds
/// and dropped otherwise, so legitimate redirects work and cross-origin ones
/// still never see the secret.
///
/// ## Method transitions
///
/// The same rules ureq applied, kept so the change is in authorization and not
/// in semantics: 303 and the historical 301/302 turn a non-GET into a GET;
/// 307/308 retain the method, and a retained method that carries a body is not
/// resent at all — the `3xx` goes back to the caller, which is what ureq did.
/// A changed method is re-checked against the grant, because a grant that
/// permits POST does not thereby permit the GET a 303 turns it into (and vice
/// versa: a read-only caller must not be walked into a write).
///
/// ## Capability tools get their declaration on every hop too (#101)
///
/// For a caller with a declaration, each hop is additionally checked against
/// it — a capability tool 302'd from a declared path to an undeclared one on
/// the SAME allowed host must be stopped by its own `path_prefixes`, or the
/// declared ∩ granted invariant holds everywhere except where an upstream
/// chooses. The hop check passes no credential name: credential *membership*
/// was settled on the initial request, and whether the header actually rides
/// a given hop is the same-origin rule's decision, not the declaration's.
/// Guest headers (#105) are settled the same way and for the same reason.
///
/// ## Guest headers travel exactly as far as the credential
///
/// One rule, not two: a header the caller attached rides while
/// `send_credential` holds and is dropped the moment the chain leaves its
/// origin. These are not secrets — the caller chose the values — so the
/// argument is not confidentiality but intent: `X-Goog-User-Project` was
/// meant for the host the caller named, and a redirect off-origin is exactly
/// where "meant for" stops being true. Sending a project id, a tenant, or an
/// API-version header onward to a destination an intermediary picked would be
/// the broker volunteering the caller's context to a stranger.
///
/// `max_response_bytes` bounds the READ of the final body, not just its
/// acceptance — `Some(n)` reads at most `n + 1` bytes, so an upstream cannot
/// make the broker buffer ten megabytes on the way to refusing one.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    start_url: &str,
    start_method: &str,
    body: Option<&str>,
    credential_headers: &[(String, String)],
    guest_headers: &[(String, String)],
    policy: &EgressPolicy,
    grant: &CallerGrant,
    declaration: Option<&Declaration>,
    max_response_bytes: Option<usize>,
    caller: &str,
    refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
) -> Dispatched {
    // A bounded agent: a connector must not be able to park the evaluator on a
    // slow upstream, which would hold the trigger's lease for the duration.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(60)))
        // The two settings this whole function exists to change.
        .max_redirects(0)
        // With auto-follow off we have to read the `3xx` ourselves, and ureq's
        // default turns every non-2xx into an `Err` carrying the status but not
        // the headers — which would put `Location` out of reach. Switching it
        // off also fixes a smaller wrong: a 404 or a 429 used to reach the
        // caller as `502 {"error": "upstream: …"}`, indistinguishable from the
        // connection having failed. The broker's contract is to answer with the
        // response, so it now does.
        .http_status_as_error(false)
        .build()
        .into();

    let mut url = start_url.to_string();
    let mut method = start_method.to_string();
    let mut body = body.map(str::to_string);
    // Once any hop leaves the starting origin the credential is retired for the
    // rest of the chain, never to return. Comparing each hop against
    // `start_url` alone is not enough: an untrusted intermediary can answer
    // `A → 302 B → 302 A/<path it chose>`, and on that final hop
    // `same_origin(A, A)` is true, so the secret would be re-attached to a
    // request an intermediary shaped. Browsers and `curl --location` drop the
    // header for good once the chain leaves the origin; this matches them.
    let mut left_origin = false;
    // The cap bounds what is READ: ureq abandons the body at the limit and
    // reports it as `BodyExceedsLimit`, which surfaces as `Err(())` here so an
    // overrun becomes a typed refusal — never an empty or truncated body
    // passed off as the upstream's answer. Other mid-body read failures keep
    // the pre-#101 behaviour (empty body with the real status).
    let read_body = |resp: &mut ureq::http::Response<ureq::Body>| -> std::result::Result<String, ()> {
        match max_response_bytes {
            Some(n) => match resp.body_mut().with_config().limit(n as u64 + 1).read_to_string() {
                Ok(text) if text.len() > n => Err(()),
                Ok(text) => Ok(text),
                Err(ureq::Error::BodyExceedsLimit(_)) => Err(()),
                Err(_) => Ok(String::new()),
            },
            None => Ok(resp.body_mut().read_to_string().unwrap_or_default()),
        }
    };

    // Inclusive: the initial request plus up to MAX_REDIRECT_HOPS follows.
    for hops in 0..=MAX_REDIRECT_HOPS as u32 {
        // The first hop is the URL the caller named and every layer above has
        // already cleared — the credential goes without a parse-dependent
        // detour. On follows, it rides only while the chain has never left the
        // starting origin (`same_origin` is then necessarily true too, but the
        // `left_origin` latch is what makes an A→B→A bounce fail closed).
        let send_credential =
            hops == 0 || (!left_origin && crate::egress::same_origin(start_url, &url));
        let result = perform(
            &agent,
            &method,
            &url,
            body.as_deref(),
            credential_headers,
            guest_headers,
            send_credential,
        );

        let mut resp = match result {
            Ok(r) => r,
            Err(e) => return Dispatched::Upstream(e.to_string()),
        };
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let Some(next_method) = redirect_method(status, &method) else {
            // Not a redirect, or one we deliberately do not follow. Either way
            // the caller gets the response as it stands.
            return match read_body(&mut resp) {
                Ok(text) => Dispatched::Answered {
                    status,
                    body: text,
                    final_url: url,
                    redirects: hops,
                    credential_sent: send_credential,
                },
                Err(()) => Dispatched::TooLarge { final_url: url },
            };
        };
        let Some(location) = location else {
            // A 3xx with no usable `Location` is not a redirect we can follow;
            // hand it back rather than invent a destination.
            return match read_body(&mut resp) {
                Ok(text) => Dispatched::Answered {
                    status,
                    body: text,
                    final_url: url,
                    redirects: hops,
                    credential_sent: send_credential,
                },
                Err(()) => Dispatched::TooLarge { final_url: url },
            };
        };
        let Some(next_url) = crate::egress::resolve_location(&url, &location) else {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.to_string(),
                    destination: location.clone(),
                    reason: "redirected to a Location that is not a resolvable http(s) URL".into(),
                },
            );
            return Dispatched::Refused {
                detail: format!(
                    "caller '{caller}' was redirected to {location:?}, which does not resolve to \
                     an http(s) URL — refused rather than guessed at"
                ),
            };
        };

        // THE check the old code did once and this does every hop.
        if let Err(e) = policy.permits(&next_url) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.to_string(),
                    destination: next_url.clone(),
                    // Worded apart from the initial refusal on purpose: "it
                    // aimed there" and "it was redirected there" are different
                    // stories for whoever reads the audit record.
                    reason: "redirect target outside the declared allowlist".into(),
                },
            );
            return Dispatched::Refused {
                detail: format!(
                    "caller '{caller}' was redirected from {url} and {e} — the redirect was not \
                     followed"
                ),
            };
        }
        // A 303 can turn a granted POST into a GET, and a 30x can turn a
        // granted GET into nothing else — but the grant is checked against
        // whatever we are about to issue, not against what was asked for.
        if !grant.permits_method(&next_method) {
            let denied = EgressDenied::Method { method: next_method.clone() };
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.to_string(),
                    destination: next_url.clone(),
                    reason: format!(
                        "redirect would issue {next_method}, which is not permitted for this caller"
                    ),
                },
            );
            return Dispatched::Refused {
                detail: format!("caller '{caller}' {denied} after a redirect"),
            };
        }
        // And the DECLARED half for a capability tool — a hop is a destination
        // the caller never named, and the declaration's host/path/method gates
        // apply to it exactly as they applied to the initial request. Without
        // this, a 302 on a declared host walks a module from its declared
        // `path_prefixes` to any endpoint the host-side grant tolerates.
        if let Some(d) = declaration {
            // The same private-space rule the initial URL got: a public,
            // policy-permitted start can 302 to the metadata service, and for
            // a capability caller under an unrestricted policy that hop needs
            // an explicit allowlist entry it does not have.
            if policy.is_unrestricted() && crate::egress::is_private_destination(&next_url) {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.to_string(),
                        destination: next_url.clone(),
                        reason: "redirected to a private or loopback destination without an \
                                 explicit --allow-host entry"
                            .into(),
                    },
                );
                return Dispatched::Refused {
                    detail: format!(
                        "caller '{caller}' was redirected from {url} to a private or loopback \
                         destination — the redirect was not followed"
                    ),
                };
            }
            if let Err(denied) = d.permits(&next_url, &next_method, None, &[]) {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.to_string(),
                        destination: next_url.clone(),
                        reason: format!("redirect outside the declared capability: {denied}"),
                    },
                );
                return Dispatched::Refused {
                    detail: format!(
                        "caller '{caller}' was redirected from {url} and {denied} — the redirect \
                         was not followed"
                    ),
                };
            }
        }

        // A method that lost its body must not carry one forward.
        if next_method != method && next_method == "GET" {
            body = None;
        }
        // A hop to a different origin retires the credential permanently: even
        // if a later hop returns to the start origin, the chain has passed
        // through somewhere untrusted that chose where it goes next.
        if !crate::egress::same_origin(start_url, &next_url) {
            left_origin = true;
        }
        method = next_method;
        url = next_url;
    }

    note_refusal(
        refusals,
        EgressRefusal {
            caller: caller.to_string(),
            destination: url.clone(),
            reason: format!("redirect chain exceeded {MAX_REDIRECT_HOPS} hops"),
        },
    );
    Dispatched::Refused {
        detail: format!(
            "caller '{caller}' followed {MAX_REDIRECT_HOPS} redirects without reaching a final \
             response — the chain was abandoned at {url}"
        ),
    }
}

/// The method to use for the hop after `status`, or `None` when this is not a
/// redirect we follow. Mirrors ureq's rules; see [`dispatch`].
fn redirect_method(status: u16, method: &str) -> Option<String> {
    match status {
        // Retaining statuses keep the method — but a method that carries a
        // body is not resent, and DELETE is excluded deliberately (repeating a
        // delete against a new URL is not obviously what anyone meant).
        307 | 308 => match method {
            "GET" | "HEAD" => Some(method.to_string()),
            _ => None,
        },
        // The historical shapes: everything that is not already a read becomes
        // a GET, which is what curl and every browser do.
        301..=303 => match method {
            "GET" | "HEAD" => Some(method.to_string()),
            _ => Some("GET".to_string()),
        },
        _ => None,
    }
}

/// One HTTP call. ureq 3 types body-carrying and body-less builders
/// differently, so the two shapes are dispatched separately rather than
/// unified behind a cast.
fn perform(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    body: Option<&str>,
    credential_headers: &[(String, String)],
    guest_headers: &[(String, String)],
    send_credential: bool,
) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    // Both sets ride or neither does — see `dispatch`'s "travel exactly as far
    // as the credential". The credential goes on LAST so that even if the two
    // ever named the same header, the broker's value is the one that survives;
    // the guest cannot reach these names at all (`is_broker_owned_header`, and
    // a `Credential::Header` name is refused beside it), so this is a belt on
    // top of braces rather than the guarantee itself.
    let credential_headers: &[(String, String)] =
        if send_credential { credential_headers } else { &[] };
    let guest_headers: &[(String, String)] = if send_credential { guest_headers } else { &[] };
    match method {
        "POST" | "PUT" | "PATCH" => {
            let mut b = match method {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                _ => agent.patch(url),
            };
            for (k, v) in guest_headers.iter().chain(credential_headers) {
                b = b.header(k, v);
            }
            b.send(body.unwrap_or(""))
        }
        _ => {
            let mut b = match method {
                "HEAD" => agent.head(url),
                "DELETE" => agent.delete(url),
                _ => agent.get(url),
            };
            for (k, v) in guest_headers.iter().chain(credential_headers) {
                b = b.header(k, v);
            }
            b.call()
        }
    }
}

/// Best-effort: read and discard up to `content_length` bytes (capped at
/// [`MAX_BODY`]) still pending on the socket.
///
/// A refusal that writes its response and drops the connection WITHOUT
/// reading a body the caller already started sending leaves those bytes
/// queued in the kernel receive buffer. Closing a socket over unread data
/// sends an RST instead of a clean FIN — which can surface to the caller as a
/// raw `ConnectionReset` on their own write, burying the 401/403 JSON body
/// under an opaque I/O error instead of a readable refusal. Draining first
/// turns that into an ordinary, parseable response every time. The cap
/// matters even here: a caller presenting a bad token gets no free pass to
/// make us read an unbounded body.
fn drain_body(reader: &mut BufReader<TcpStream>, content_length: usize) {
    let mut discard = vec![0u8; content_length.min(MAX_BODY)];
    let _ = reader.read_exact(&mut discard);
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} \r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(broker: &Broker, req: serde_json::Value) -> (u16, serde_json::Value) {
        call_as(broker, "connector", req)
    }

    /// Call presenting `caller`'s token, or a bogus one if it has no grant.
    fn call_as(broker: &Broker, caller: &str, req: serde_json::Value) -> (u16, serde_json::Value) {
        let token = broker.token_for(caller).unwrap_or("not-a-real-token").to_string();
        call_with_token(broker, &token, req)
    }

    fn call_with_token(
        broker: &Broker,
        token: &str,
        req: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let addr = broker.url().trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = req.to_string();
        let head = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nX-Areev-Egress-Token: {token}\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body.as_bytes()).unwrap();
        s.flush().unwrap();

        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status: u16 = raw
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("{}");
        (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
    }

    /// The connector-shaped setup the existing cases assume: one caller, all
    /// methods, every configured credential.
    fn grants(creds: &[&str]) -> EgressGrants {
        let g = creds.iter().fold(
            CallerGrant::new().method("GET").method("POST").method("DELETE"),
            |g, c| g.credential(c),
        );
        EgressGrants::new().default_for_all(g)
    }

    fn policy(entries: &[&str]) -> EgressPolicy {
        EgressPolicy::from_config(Some(&serde_json::json!({
            "int:allowed_outbound_hosts": entries
        })))
        .unwrap()
    }

    #[test]
    fn a_disallowed_destination_is_refused_before_any_request_is_made() {
        // The point: refusal happens here, so a connector aiming somewhere it
        // should not never gets a socket to that host at all.
        let b = Broker::start(policy(&["https://api.github.com"]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, body) = call(&b, serde_json::json!({ "url": "https://evil.com/steal" }));
        assert_eq!(status, 403);
        assert_eq!(body["code"], "TRG-E009");
        let refusals = b.refusals();
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].destination, "https://evil.com/steal");
        assert!(refusals[0].reason.contains("allowlist"), "{:?}", refusals[0]);
    }

    #[test]
    fn a_caller_cannot_name_a_credential_its_grant_does_not_cover() {
        // Naming a credential is not the same as being allowed to use it.
        let b = Broker::start(
            policy(&["https://api.github.com"]),
            BTreeMap::new(),
            grants(&[]),
            "TRG-E009",
        )
        .unwrap();
        let (status, body) = call(
            &b,
            serde_json::json!({ "url": "https://api.github.com/x", "credential": "gmail" }),
        );
        assert_eq!(status, 403);
        assert!(body["error"].as_str().unwrap().contains("may not use credential"), "{body}");
    }

    #[test]
    fn a_granted_credential_that_the_host_never_configured_is_a_client_error() {
        // Distinct from the case above: the grant permits the name, but no
        // value was configured. That is the host's mistake, not the caller's
        // overreach, and the status says which.
        let b = Broker::start(
            policy(&["https://api.github.com"]),
            BTreeMap::new(),
            grants(&["gmail"]),
            "TRG-E009",
        )
        .unwrap();
        let (status, body) = call(
            &b,
            serde_json::json!({ "url": "https://api.github.com/x", "credential": "gmail" }),
        );
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("no credential named"), "{body}");
    }

    #[test]
    fn a_malformed_request_is_a_client_error_not_a_panic() {
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, _) = call(&b, serde_json::json!("not an egress request"));
        assert_eq!(status, 400);
    }

    #[test]
    fn a_caller_with_no_token_is_refused_before_its_body_is_parsed() {
        // Loopback is not an authorization: without a token, any process on
        // the box could spend the credentials this broker holds.
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let addr = b.url().trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = "not json";
        s.write_all(
            format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                .as_bytes(),
        )
        .unwrap();
        s.flush().unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        assert!(raw.contains("401"), "{raw}");
    }

    #[test]
    fn a_forged_token_is_refused() {
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, _) =
            call_with_token(&b, "0".repeat(48).as_str(), serde_json::json!({ "url": "https://x/" }));
        assert_eq!(status, 401);
    }

    /// A refusal must not corrupt the caller's own write with an RST (found
    /// while releasing 1.3.1: a `ConnectionReset` here once, under heavy
    /// concurrent-build load).
    ///
    /// The tiny bodies elsewhere in this file fit entirely inside the OS
    /// socket buffers, so `write_all` never blocks and completes before the
    /// server has a chance to close — the race that causes an RST only shows
    /// up under enough scheduling delay to matter, which is exactly why it
    /// took heavy system load to surface and why a normal CI run would not
    /// reliably catch a regression here. A body large enough to force real
    /// TCP backpressure reproduces the same race on every run, load or not:
    /// with the server closing before draining, this `write_all` fails with
    /// `ConnectionReset`; with it draining first, the write always completes
    /// and the 401 is always readable. Verified: this test fails
    /// deterministically (no stress needed) against the code before the
    /// `drain_body` fix, on the very first run.
    #[test]
    fn a_refusal_drains_the_body_so_a_large_write_never_resets() {
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        // Comfortably under MAX_BODY (drain_body covers the whole thing) and
        // comfortably over typical default OS socket buffer sizes, so the
        // write backpressures for real rather than completing instantly.
        let pad = "a".repeat(900_000);
        let (status, _) =
            call_with_token(&b, "0".repeat(48).as_str(), serde_json::json!({ "pad": pad }));
        assert_eq!(status, 401);
    }

    #[test]
    fn a_grant_naming_no_method_may_only_read() {
        // Connectors read; tools write. The write verb is always something
        // someone decided to allow.
        let b = Broker::start(
            EgressPolicy::unrestricted(),
            BTreeMap::new(),
            EgressGrants::new().grant("reader", CallerGrant::new()),
            "TRG-E009",
        )
        .unwrap();
        let (status, body) = call_as(
            &b,
            "reader",
            serde_json::json!({ "url": "https://example.com/x", "method": "POST" }),
        );
        assert_eq!(status, 403);
        assert!(body["error"].as_str().unwrap().contains("not permitted"), "{body}");
    }

    #[test]
    fn one_callers_token_does_not_buy_anothers_scope() {
        // Two tools, one broker, one port: the token is what tells them apart.
        let b = Broker::start(
            EgressPolicy::unrestricted(),
            BTreeMap::new(),
            EgressGrants::new()
                .grant("writer", CallerGrant::new().method("POST").credential("zoho"))
                .grant("reader", CallerGrant::new()),
            "TRG-E009",
        )
        .unwrap();
        assert_ne!(b.token_for("writer").unwrap(), b.token_for("reader").unwrap());

        // The reader presenting its own token cannot POST...
        let (status, _) = call_as(
            &b,
            "reader",
            serde_json::json!({ "url": "https://example.com/x", "method": "POST" }),
        );
        assert_eq!(status, 403);

        // ...and cannot reach for the writer's credential either.
        let (status, _) = call_as(
            &b,
            "reader",
            serde_json::json!({ "url": "https://example.com/x", "credential": "zoho" }),
        );
        assert_eq!(status, 403);
    }

    #[test]
    fn a_caller_with_no_grant_at_all_gets_no_token() {
        let b = Broker::start(
            EgressPolicy::unrestricted(),
            BTreeMap::new(),
            EgressGrants::new().grant("writer", CallerGrant::new()),
            "TRG-E009",
        )
        .unwrap();
        assert!(b.token_for("nobody").is_none(), "an ungranted caller must not see the broker");
    }

    #[test]
    fn the_broker_binds_loopback_only() {
        // A service that holds credentials must not be reachable off-box.
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        assert!(b.url().starts_with("http://127.0.0.1:"), "{}", b.url());
    }

    #[test]
    fn credentials_come_from_a_named_variable_never_a_literal() {
        std::env::set_var("AREEV_TEST_BROKER_TOKEN", "s3cret");
        let c = Credential::bearer_from_env("AREEV_TEST_BROKER_TOKEN").unwrap();
        std::env::remove_var("AREEV_TEST_BROKER_TOKEN");
        assert!(matches!(c, Credential::Bearer(v) if v == "s3cret"));

        // Unset and empty both refuse: a broker that silently attaches nothing
        // sends unauthenticated requests that fail confusingly upstream.
        assert!(Credential::bearer_from_env("AREEV_TEST_BROKER_ABSENT").is_err());
        std::env::set_var("AREEV_TEST_BROKER_EMPTY", "   ");
        assert!(Credential::bearer_from_env("AREEV_TEST_BROKER_EMPTY").is_err());
        std::env::remove_var("AREEV_TEST_BROKER_EMPTY");
    }

    #[test]
    fn an_unrestricted_policy_still_brokers_rather_than_handing_over_the_token() {
        // Even with no allowlist, the credential stays here — the connector
        // gets a URL, not a secret.
        let b = Broker::start(EgressPolicy::unrestricted(), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, _) = call(&b, serde_json::json!({ "url": "https://anywhere.example/" }));
        assert_ne!(status, 403, "an absent allowlist does not refuse");
    }
}
