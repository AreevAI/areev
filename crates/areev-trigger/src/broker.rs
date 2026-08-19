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

use crate::egress::EgressPolicy;
use crate::error::{Result, TriggerError};

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
        let v = std::env::var(var).map_err(|_| TriggerError::Malformed {
            what: format!("credential env var {var} is not set"),
        })?;
        if v.trim().is_empty() {
            return Err(TriggerError::Malformed {
                what: format!("credential env var {var} is empty"),
            });
        }
        Ok(Credential::Bearer(v))
    }
}

/// A running broker. Dropping it stops the listener.
pub struct Broker {
    url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Destinations refused during this pass, for journalling.
    refusals: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Broker {
    /// Start a broker on an ephemeral loopback port.
    pub fn start(policy: EgressPolicy, credentials: BTreeMap<String, Credential>) -> Result<Broker> {
        // 127.0.0.1 explicitly, never 0.0.0.0: a broker that holds credentials
        // and is reachable off-box is a credential server.
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| TriggerError::Storage {
            detail: format!("egress broker cannot bind loopback: {e}"),
        })?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        listener.set_nonblocking(true).ok();

        let stop = Arc::new(AtomicBool::new(false));
        let refusals = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop_t, refusals_t) = (Arc::clone(&stop), Arc::clone(&refusals));

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
                        let _ = serve_one(stream, &policy, &credentials, &refusals_t);
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
        })
    }

    /// What to put in the connector's `AREEV_EGRESS_URL`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Destinations this pass refused. Journaled so a connector reaching for
    /// somewhere it should not is visible rather than merely failing.
    pub fn refusals(&self) -> Vec<String> {
        self.refusals.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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

fn serve_one(
    mut stream: TcpStream,
    policy: &EgressPolicy,
    credentials: &BTreeMap<String, Credential>,
    refusals: &Arc<std::sync::Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    let mut reader = BufReader::new(stream.try_clone()?);

    // Request line, then headers, then a Content-Length body.
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY {
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
        if let Ok(mut r) = refusals.lock() {
            r.push(req.url.clone());
        }
        return respond(
            &mut stream,
            403,
            &serde_json::json!({ "error": e.to_string(), "code": "TRG-E009" }).to_string(),
        );
    }

    let method = if req.method.trim().is_empty() { "GET" } else { req.method.trim() };
    let method = method.to_ascii_uppercase();

    // Resolve the credential to a header pair before dispatch. The connector
    // chooses WHICH credential by name; it can never name a value it was not
    // given, and no value ever crosses back to it.
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(name) = &req.credential {
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
        let addr = broker.url().trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = req.to_string();
        let head = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
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
        let b = Broker::start(policy(&["https://api.github.com"]), BTreeMap::new()).unwrap();
        let (status, body) = call(&b, serde_json::json!({ "url": "https://evil.com/steal" }));
        assert_eq!(status, 403);
        assert_eq!(body["code"], "TRG-E009");
        assert_eq!(b.refusals(), vec!["https://evil.com/steal".to_string()]);
    }

    #[test]
    fn the_connector_cannot_name_a_credential_it_was_not_given() {
        let b = Broker::start(policy(&["https://api.github.com"]), BTreeMap::new()).unwrap();
        let (status, body) = call(
            &b,
            serde_json::json!({ "url": "https://api.github.com/x", "credential": "gmail" }),
        );
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("no credential named"));
    }

    #[test]
    fn a_malformed_request_is_a_client_error_not_a_panic() {
        let b = Broker::start(policy(&[]), BTreeMap::new()).unwrap();
        let addr = b.url().trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = "not json";
        s.write_all(
            format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes(),
        )
        .unwrap();
        s.flush().unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        assert!(raw.contains("400"), "{raw}");
    }

    #[test]
    fn the_broker_binds_loopback_only() {
        // A service that holds credentials must not be reachable off-box.
        let b = Broker::start(policy(&[]), BTreeMap::new()).unwrap();
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
        let b = Broker::start(EgressPolicy::unrestricted(), BTreeMap::new()).unwrap();
        let (status, _) = call(&b, serde_json::json!({ "url": "https://anywhere.example/" }));
        assert_ne!(status, 403, "an absent allowlist does not refuse");
    }
}
