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
//!   "method": "GET", "credential": "gmail" }
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
    pub fn bearer_from_env(var: &str) -> Result<Credential> {
        let v = std::env::var(var)
            .map_err(|_| format!("credential env var {var} is not set"))?;
        if v.trim().is_empty() {
            return Err(format!("credential env var {var} is empty"));
        }
        Ok(Credential::Bearer(v))
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

/// A running broker. Dropping it stops the listener.
pub struct Broker {
    url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Destinations refused during this pass, for journalling.
    refusals: Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
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
        let (stop_t, refusals_t) = (Arc::clone(&stop), Arc::clone(&refusals));

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
            tokens,
            default_token,
        })
    }

    /// The capability token `caller` presents, if it has a grant.
    pub fn token_for(&self, caller: &str) -> Option<&str> {
        self.tokens
            .get(caller)
            .or(self.default_token.as_ref())
            .map(String::as_str)
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

fn serve_one(
    mut stream: TcpStream,
    policy: &EgressPolicy,
    credentials: &BTreeMap<String, Credential>,
    refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
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

    // A bounded agent: a connector must not be able to park the evaluator on a
    // slow upstream, which would hold the trigger's lease for the duration.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();

    // ureq 3 types body-carrying and body-less builders differently, so the
    // two shapes are dispatched separately rather than unified behind a cast.
    let result = match method.as_str() {
        "GET" | "DELETE" => {
            let mut b = if method == "GET" { agent.get(&req.url) } else { agent.delete(&req.url) };
            for (k, v) in &headers {
                b = b.header(k, v);
            }
            b.call()
        }
        "POST" | "PUT" | "PATCH" => {
            let mut b = match method.as_str() {
                "POST" => agent.post(&req.url),
                "PUT" => agent.put(&req.url),
                _ => agent.patch(&req.url),
            };
            for (k, v) in &headers {
                b = b.header(k, v);
            }
            b.send(req.body.as_deref().unwrap_or(""))
        }
        other => {
            return respond(
                &mut stream,
                400,
                &serde_json::json!({ "error": format!("unsupported method '{other}'") }).to_string(),
            )
        }
    };

    match result {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            respond(
                &mut stream,
                200,
                &serde_json::json!({ "status": status, "body": text }).to_string(),
            )
        }
        Err(e) => respond(
            &mut stream,
            502,
            &serde_json::json!({ "error": format!("upstream: {e}") }).to_string(),
        ),
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
