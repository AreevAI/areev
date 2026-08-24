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
/// `(method, path, authorization, all headers)` — what one request looked like.
///
/// The fourth element is every header the upstream received, lowercased names,
/// in arrival order: `authorization` is called out separately because most of
/// this suite is about whether the *credential* travelled, but #105 lets a
/// caller set its own headers and "did `X-Goog-User-Project` actually arrive"
/// is answerable only here, at the socket the broker wrote to.
type SeenRequest = (String, String, Option<String>, Vec<(String, String)>);
/// `(path, status, extra headers, body)` — one row of the routing table.
type Route = (&'static str, u16, Vec<(String, String)>, &'static str);

struct Upstream {
    port: u16,
    seen: Arc<std::sync::Mutex<Vec<SeenRequest>>>,
    routes: Arc<std::sync::Mutex<Vec<Route>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Upstream {
    fn start(routes: Vec<Route>) -> Upstream {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let routes = Arc::new(std::sync::Mutex::new(routes));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen_t, routes_t, stop_t) = (Arc::clone(&seen), Arc::clone(&routes), Arc::clone(&stop));

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
                let mut all_headers: Vec<(String, String)> = Vec::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some((k, v)) = h.split_once(':') {
                        all_headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
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
                seen_t.lock().unwrap().push((method, path.clone(), auth, all_headers));

                let (status, headers, body) = {
                    let routes = routes_t.lock().unwrap();
                    match routes.iter().find(|(p, ..)| *p == path) {
                        Some((_, s, h, b)) => (*s, h.clone(), *b),
                        None => (404, Vec::new(), "not found"),
                    }
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
        Upstream { port, seen, routes, stop }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Add a route after start — for chains whose target port is only known
    /// once the peer server is bound (an `A → B → A` bounce, for one).
    fn add_route(&self, route: Route) {
        self.routes.lock().unwrap().push(route);
    }

    /// Every request this server handled, in order.
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
        [("zoho".to_string(), areev_run::Credential::bearer_from_env("AREEV_TEST_ZOHO").unwrap().into())]
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
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_REDIR_CRED").unwrap().into())]
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
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_XORIGIN_CRED").unwrap().into())]
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

/// Once the chain has left the starting origin the credential is gone for good,
/// even if a later hop returns to that origin. An `A → B → A` bounce is exactly
/// how an untrusted intermediary (`B`) would try to have the secret re-attached
/// to a path it, not the caller, chose — so the final `A` request must arrive
/// unauthenticated, and the audit must not claim the credential was used.
#[test]
fn a_credential_is_not_re_attached_after_the_chain_leaves_and_returns_to_the_origin() {
    // Both servers are bound before their cross-referencing routes are set, so
    // each Location can name the other's real port.
    let first = Upstream::start(vec![("/return", 200, Vec::new(), "home again")]);
    let other = Upstream::start(vec![]);
    first.add_route(("/here", 302, header("Location", &format!("{}/detour", other.origin())), ""));
    other.add_route(("/detour", 302, header("Location", &format!("{}/return", first.origin())), ""));

    std::env::set_var("AREEV_TEST_BOUNCE_CRED", "do-not-return");
    let broker = Broker::start(
        policy_for(&[first.origin(), other.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_BOUNCE_CRED").unwrap().into())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_BOUNCE_CRED");
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/here", first.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["body"], json!("home again"), "the bounce was followed to the end");

    let reqs = first.requests();
    assert_eq!(reqs.len(), 2, "both first-origin hops happened: {reqs:?}");
    assert_eq!(reqs[0].1, "/here");
    assert_eq!(reqs[0].2.as_deref(), Some("bearer do-not-return"), "the caller's own hop is authed");
    assert_eq!(reqs[1].1, "/return");
    assert_eq!(
        reqs[1].2, None,
        "the credential must NOT come back after the chain passed through another origin: {reqs:?}"
    );

    // And the audit must not claim a credential reached the final destination.
    let calls = broker.calls();
    assert_eq!(calls.len(), 1, "one successful call recorded");
    assert_eq!(
        calls[0].credential, None,
        "the audit records no credential when the final hop carried none: {:?}",
        calls[0]
    );
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
            [("api".to_string(), cred.into())].into_iter().collect(),
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
    let paths: Vec<String> = site.requests().into_iter().map(|(_, p, _, _)| p).collect();
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
        [("gmail".to_string(), Credential::bearer_from_env("AREEV_TEST_UNGRANTED").unwrap().into())]
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
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_RECORDED").unwrap().into())]
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
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_REFLECT").unwrap().into())]
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

// ---- #105: non-credential request headers ----------------------------------

/// The use case the feature exists for: a declared header reaches the upstream.
///
/// Every Google API called with user credentials wants `X-Goog-User-Project`
/// or answers 403, and that header is not a credential — which is exactly why
/// the broker could not set it and the guest could not either. Asserted at the
/// socket, because "the broker accepted the request" is not the same claim as
/// "the header arrived".
#[test]
fn a_declared_header_reaches_the_upstream() {
    let site = Upstream::start(vec![("/v4/sheets/append", 200, Vec::new(), "appended")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("append_sheet", CallerGrant::new().method("POST")),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "append_sheet",
        declaration(json!([{"http": {
            "hosts": [site.origin()],
            "methods": ["POST"],
            "headers": ["X-Goog-User-Project"]
        }}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("append_sheet").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({
            "url": format!("{}/v4/sheets/append", site.origin()),
            "method": "POST",
            "headers": { "X-Goog-User-Project": "my-project" },
            "body": "{}"
        }),
    );
    assert_eq!(code, 200, "the call goes out: {body}");

    let reqs = site.requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].3.iter().any(|(k, v)| k == "x-goog-user-project" && v == "my-project"),
        "the declared header arrived at the upstream: {:?}",
        reqs[0].3
    );
}

/// An undeclared header is refused, on the same deny-by-default reading the
/// method and credential sets get: declaring some headers does not declare all
/// of them, and declaring none declares none.
#[test]
fn an_undeclared_header_is_refused() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new().method("POST")),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"http": {
            "hosts": [site.origin()],
            "methods": ["POST"],
            "headers": ["X-Goog-User-Project"]
        }}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({
            "url": format!("{}/x", site.origin()),
            "method": "POST",
            "headers": { "X-Tenant-Id": "acme" },
            "body": "{}"
        }),
    );
    assert_eq!(code, 403, "an undeclared header is refused: {body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("X-Tenant-Id"),
        "and the refusal names it: {body}"
    );
    assert!(site.requests().is_empty(), "nothing went out");
}

/// The load-bearing refusal: the guest may not write the credential channel.
///
/// Letting a module set `Authorization` would hand back exactly the surface
/// brokering exists to remove — it could attach a credential it minted, or
/// overwrite the one the broker attached. Checked for every spelling, because
/// HTTP field names are case-insensitive and a check that is not would be
/// bypassed by `authorization`. `Host` is here too: it re-targets the request
/// *after* the allowlist judged the URL.
#[test]
fn a_guest_cannot_set_the_headers_the_broker_owns() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    std::env::set_var("AREEV_TEST_OWNED_HDR", "s3cret-value");
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_OWNED_HDR").unwrap().into())]
            .into_iter()
            .collect(),
        EgressGrants::new()
            .grant("t", CallerGrant::new().method("POST").credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    for name in ["Authorization", "authorization", "AUTHORIZATION", "Cookie", "Host",
                 "Proxy-Authorization"]
    {
        let (code, body) = ask(
            broker.url(),
            &token,
            json!({
                "url": format!("{}/x", site.origin()),
                "method": "POST",
                "headers": { name: "attacker-chosen" },
                "body": "{}"
            }),
        );
        assert_eq!(code, 403, "'{name}' is the broker's to set: {body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("broker owns"),
            "and the refusal says so: {body}"
        );
    }
    assert!(site.requests().is_empty(), "not one of them went out");
}

/// A credential carried in a NON-`Authorization` header is protected too.
///
/// `Credential::Header` lets an operator put a secret in any header the
/// upstream wants — `X-Api-Key`, say — and that name is host configuration, so
/// it cannot be in a static list. The collision is caught after resolution,
/// which is the only place the name is known.
#[test]
fn a_guest_cannot_overwrite_a_header_carrying_a_configured_credential() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [(
            "api".to_string(),
            Credential::Header { name: "X-Api-Key".into(), value: "s3cret".into() }.into(),
        )]
        .into_iter()
        .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().method("POST").credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    // Different casing from the configured name, to pin that the check is
    // case-insensitive rather than a string equality that happens to pass.
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({
            "url": format!("{}/x", site.origin()),
            "method": "POST",
            "credential": "api",
            "headers": { "x-api-key": "attacker-chosen" },
            "body": "{}"
        }),
    );
    assert_eq!(code, 403, "the credential's own header is not the guest's to write: {body}");
    assert!(site.requests().is_empty(), "nothing went out");
}

/// Header injection: a CR or LF in a value splits one request into two, and
/// the second one is the caller's to shape. Refused as malformed, before any
/// policy question is asked.
#[test]
fn a_header_value_carrying_crlf_is_refused_as_malformed() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "ok")]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new().method("POST")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    for (name, value) in [
        ("X-Evil", "ok\r\nX-Injected: yes"),
        ("X-Evil", "ok\nX-Injected: yes"),
        ("X-Bad Name", "fine"),
        ("X:Colon", "fine"),
    ] {
        let (code, _) = ask(
            broker.url(),
            &token,
            json!({
                "url": format!("{}/x", site.origin()),
                "method": "POST",
                "headers": { name: value },
                "body": "{}"
            }),
        );
        assert_eq!(code, 400, "'{name}: {value}' is malformed, not merely refused");
    }
    assert!(site.requests().is_empty(), "nothing went out");
}

/// Guest headers travel exactly as far as the credential: a cross-origin
/// redirect drops both.
///
/// Not a secrecy argument — the caller chose these values — but an intent one.
/// The header was meant for the host the caller named; a redirect off-origin
/// is where "meant for" stops being true, and volunteering a quota project or
/// tenant id to a destination an intermediary picked is the broker leaking the
/// caller's context.
#[test]
fn guest_headers_do_not_survive_a_cross_origin_redirect() {
    let elsewhere = Upstream::start(vec![("/landing", 200, Vec::new(), "landed")]);
    let start = Upstream::start(vec![]);
    start.add_route((
        "/go",
        302,
        header("Location", &format!("{}/landing", elsewhere.origin())),
        "",
    ));
    let broker = Broker::start(
        policy_for(&[start.origin(), elsewhere.origin()]),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({
            "url": format!("{}/go", start.origin()),
            "method": "GET",
            "headers": { "X-Goog-User-Project": "my-project" }
        }),
    );
    assert_eq!(code, 200, "the chain completes: {body}");

    let first = start.requests();
    assert!(
        first[0].3.iter().any(|(k, _)| k == "x-goog-user-project"),
        "it rode the hop the caller named: {:?}",
        first[0].3
    );
    let second = elsewhere.requests();
    assert_eq!(second.len(), 1, "the redirect was followed");
    assert!(
        !second[0].3.iter().any(|(k, _)| k == "x-goog-user-project"),
        "but not the one an intermediary chose: {:?}",
        second[0].3
    );
}

/// The audit half: headers are journaled with their VALUES, unlike the
/// credential which is journaled by name only.
///
/// The asymmetry is the point. A credential value is the host's secret and an
/// Observation is immutable and replicates, so it may never carry one. A guest
/// header value came from the caller, discloses nothing the caller did not
/// already hold, and turns "it was allowed to reach Google" into "it billed
/// this quota project on these four requests".
// Unix-only for the same reason as every other shell-driven test here: the
// vehicle is a `curl` pipeline with `$VAR` expansion and single-quoted JSON,
// which `cmd /C` does not parse. The FEATURE is platform-independent and is
// covered everywhere by the socket-level tests above.
#[cfg(unix)]
#[test]
fn guest_headers_are_journaled_with_their_values() {
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
    let exec = CommandExecutor::new(&format!(
        "curl -s -X POST -H \"X-Areev-Egress-Token: $AREEV_EGRESS_TOKEN\" \
           -d '{{\"url\":\"{origin}/ok\",\"method\":\"GET\",\
                 \"headers\":{{\"X-Goog-User-Project\":\"my-project\"}}}}' \
           \"$AREEV_EGRESS_URL\" >/dev/null; \
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
            "r-hdrs",
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
    let call = obs
        .iter()
        .find(|g| g.get_str("observation_kind") == Some("egress_call"))
        .expect("the call was journaled");

    let rendered = format!("{call:?}");
    assert!(
        rendered.contains("X-Goog-User-Project") && rendered.contains("my-project"),
        "the header rides the audit trail, name and value: {rendered}"
    );
}

// ---- #106: reading CAS blobs through the same broker ----------------------

/// Post a blob request to the broker's `/blob` path, returning
/// `(http status, raw body)`. Raw, not JSON: a success answer is the blob's
/// own bytes, which is the whole point of that door.
fn ask_blob(broker_url: &str, token: &str, uri: &str) -> (u16, Vec<u8>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let addr = broker_url.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let body = json!({ "uri": uri }).to_string();
    let head = format!(
        "POST /blob HTTP/1.1\r\nHost: {addr}\r\nX-Areev-Egress-Token: {token}\r\n\
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
    (status, buf)
}

/// A memory with one blob in it, and the CAS uri that addresses it.
fn memory_with_blob(dir: &std::path::Path, bytes: &[u8]) -> (String, String) {
    use areev_store::Areev;
    let path = dir.join("m.db").to_str().unwrap().to_string();
    let mut m = Areev::open(&path).unwrap();
    let uri = m.put_blob(bytes).unwrap();
    drop(m);
    (path, uri)
}

/// The use case: a declared module reads the attachment it was handed, and
/// gets the bytes themselves rather than JSON wrapping them.
#[test]
fn a_declared_module_reads_a_blob_through_the_broker() {
    let dir = tempfile::TempDir::new().unwrap();
    // Deliberately not valid UTF-8 and deliberately starting with `{`: a real
    // attachment is arbitrary bytes, and anything that sniffed the payload to
    // decide "is this an error?" would misread exactly this.
    let payload = b"{\x00\x01\x02 not json, not utf8 \xff\xfe".to_vec();
    let (db, uri) = memory_with_blob(dir.path(), &payload);

    let broker = Broker::start(
        EgressPolicy::default(),
        Default::default(),
        EgressGrants::new().grant("parse_attachments", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.serve_blobs(&db);
    broker.declare(
        "parse_attachments",
        declaration(json!([{"blob": {"read": true}}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("parse_attachments").unwrap().to_string();

    let (code, body) = ask_blob(broker.url(), &token, &uri);
    assert_eq!(code, 200);
    assert_eq!(body, payload, "the bytes arrive verbatim, not re-encoded");

    let reads = broker.blob_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].caller, "parse_attachments");
    assert_eq!(reads[0].uri, uri);
    assert_eq!(reads[0].bytes, payload.len());
}

/// Declaring is the only key, so not declaring is refused — including for the
/// callers that CAN reach the network. An http capability is not a blob one.
#[test]
fn an_undeclared_module_cannot_read_a_blob() {
    let dir = tempfile::TempDir::new().unwrap();
    let (db, uri) = memory_with_blob(dir.path(), b"secret attachment");

    let broker = Broker::start(
        EgressPolicy::default(),
        Default::default(),
        EgressGrants::new()
            .grant("http_only", CallerGrant::new().method("POST"))
            .grant("plain", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.serve_blobs(&db);
    // Declares http, and nothing about blobs.
    broker.declare(
        "http_only",
        declaration(json!([{"http": {"hosts": ["https://api.example.com"], "methods": ["POST"]}}])),
        CapabilityLimits::default(),
    );

    for caller in ["http_only", "plain"] {
        let token = broker.token_for(caller).unwrap().to_string();
        let (code, body) = ask_blob(broker.url(), &token, &uri);
        assert_eq!(code, 403, "'{caller}' did not declare the blob capability");
        assert!(
            !body.windows(6).any(|w| w == b"secret"),
            "and got none of the bytes"
        );
    }
    assert!(broker.blob_reads().is_empty(), "nothing was read");
}

/// A module may fetch bytes it was handed a reference to, and cannot go
/// looking for others: the address is the only way in, and a malformed or
/// unknown one is an error rather than a browse.
#[test]
fn a_blob_read_is_by_content_address_only() {
    let dir = tempfile::TempDir::new().unwrap();
    let (db, _uri) = memory_with_blob(dir.path(), b"stored");

    let broker = Broker::start(
        EgressPolicy::default(),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.serve_blobs(&db);
    broker.declare(
        "t",
        declaration(json!([{"blob": {"read": true}}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();

    for bad in [
        "",
        "not-a-uri",
        "file:///etc/passwd",
        "../../etc/passwd",
        "cas://sha256:short",
        // A well-formed address for a blob this memory does not hold.
        "cas://sha256:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        let (code, _) = ask_blob(broker.url(), &token, bad);
        assert_eq!(code, 404, "{bad:?} is not readable");
    }
    assert!(broker.blob_reads().is_empty(), "and none of them counted as a read");
}

/// Declared, but the host wired no memory: refused rather than served from
/// somewhere unexpected. Declaring is not granting on this door either.
#[test]
fn a_blob_read_needs_the_host_to_have_wired_a_memory() {
    let broker = Broker::start(
        EgressPolicy::default(),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([{"blob": {"read": true}}])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();
    let (code, _) = ask_blob(
        broker.url(),
        &token,
        "cas://sha256:0000000000000000000000000000000000000000000000000000000000000000",
    );
    assert_eq!(code, 503, "no memory wired, so nothing to read");
}

/// The token is the caller's identity on this door too: an unauthenticated
/// process on the same box gets nothing.
#[test]
fn a_blob_read_without_a_token_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let (db, uri) = memory_with_blob(dir.path(), b"attachment");
    let broker = Broker::start(
        EgressPolicy::default(),
        Default::default(),
        EgressGrants::new().grant("t", CallerGrant::new()),
        "RUN-E022",
    )
    .unwrap();
    broker.serve_blobs(&db);
    broker.declare(
        "t",
        declaration(json!([{"blob": {"read": true}}])),
        CapabilityLimits::default(),
    );

    let (code, _) = ask_blob(broker.url(), "not-a-real-token", &uri);
    assert_eq!(code, 401);
    assert!(broker.blob_reads().is_empty());
}

/// The audit half, end to end: a blob a tool read is auditable from the
/// memory, on the same superstep boundary as a brokered call.
///
/// This is the property that made routing blob reads through the broker the
/// right design rather than reading the sidecar inside the sandbox: a read
/// performed in the subprocess has no way back to the driver to be journaled,
/// and the tool this capability exists for is the one that parses untrusted
/// attachments — precisely where a hole in the audit trail is least
/// affordable.
// Unix-only: same `curl`-through-a-POSIX-shell vehicle as its `egress_call`
// sibling. The blob door itself is exercised on every platform by
// `a_declared_module_reads_a_blob_through_the_broker`.
#[cfg(unix)]
#[test]
fn a_blob_read_is_auditable_from_the_memory() {
    use areev_cal::AreevFacade;
    use areev_core::types::{Grain, Tool, ToolKind, Workflow};
    use areev_run::{CommandExecutor, EgressHandle, RunOptions, Runner, ScriptedClock};
    use areev_store::Areev;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db").to_str().unwrap().to_string();
    let mut m = Areev::open(&db).unwrap();
    let uri = m.put_blob(b"an invoice PDF, pretend").unwrap();
    let facade = Arc::new(AreevFacade::new(m));

    let def = Tool::new("parse_attachments")
        .kind(ToolKind::Definition)
        .tool_description("reads stored bytes")
        .created_at(500)
        .namespace("ops");
    let dh = facade.with_store(|m| m.add(&def)).unwrap();
    let plan = facade
        .with_store(|m| {
            m.add(
                &Workflow::new(vec!["parse_attachments".into()])
                    .bind("parse_attachments", &dh.to_hex())
                    .created_at(600)
                    .namespace("ops"),
            )
        })
        .unwrap();

    // A grant naming neither a credential nor a method: it mints the token
    // that identifies the caller and authorizes no egress at all — the shape
    // a module that only reads attachments takes.
    let broker = Arc::new(
        Broker::start(
            EgressPolicy::default(),
            Default::default(),
            EgressGrants::new().grant("parse_attachments", CallerGrant::new()),
            "RUN-E022",
        )
        .unwrap(),
    );
    broker.serve_blobs(&db);
    broker.declare(
        "parse_attachments",
        declaration(json!([{"blob": {"read": true}}])),
        CapabilityLimits::default(),
    );

    let exec = CommandExecutor::new(&format!(
        "curl -s -X POST -H \"X-Areev-Egress-Token: $AREEV_EGRESS_TOKEN\" \
           -d '{{\"uri\":\"{uri}\"}}' \"$AREEV_EGRESS_URL/blob\" >/dev/null; \
         echo '{{\"parsed\":true}}'"
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
            "r-blob",
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
    let read = obs
        .iter()
        .find(|g| g.get_str("observation_kind") == Some("blob_read"))
        .expect("the read was journaled");

    assert_eq!(read.get_str("run_id"), Some("r-blob"));
    assert_eq!(read.get_str("caller"), Some("parse_attachments"));
    assert_eq!(read.get_str("blob"), Some(uri.as_str()));
    // The address names the bytes; the bytes themselves are not in the grain.
    assert!(
        !format!("{read:?}").contains("an invoice PDF"),
        "the content must not ride an immutable replicating grain: {read:?}"
    );
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
    let paths: Vec<String> = site.requests().into_iter().map(|(_, p, _, _)| p).collect();
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
        [("api".to_string(), cred.into())].into_iter().collect(),
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
        [("api".to_string(), cred.into())].into_iter().collect(),
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
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_UNOWNED").unwrap().into())]
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
    // A local service standing in for the console / metadata endpoint.
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
            [("api".to_string(), cred.into())].into_iter().collect(),
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

// ---- #112: a credential is bound to a host, not merely to a caller ---------

use areev_run::{AllowedHost, CredentialSource};

/// The exposure, stated as a test: a tool that legitimately holds two
/// services' credentials must not be able to send one service's secret to the
/// other.
///
/// Both servers are on the allowlist and the caller is granted both
/// credentials, which is exactly the shape that used to permit this — `hosts`
/// and `credentials` were independent membership tests with nothing relating
/// them. The pairing is what refuses it, and it is the HOST-side half: the
/// declaration travels with the tool, so it cannot be the only thing deciding
/// where a secret may go.
///
/// `localhost` and `127.0.0.1` name the two hosts because both test servers
/// bind loopback — different names, genuinely different hosts to the matcher.
#[test]
fn a_paired_credential_is_refused_at_a_host_it_was_not_paired_with() {
    let mail = Upstream::start(vec![("/m", 200, Vec::new(), "{\"ok\":true}")]);
    let sheets = Upstream::start(vec![("/s", 200, Vec::new(), "{\"ok\":true}")]);
    let mail_origin = format!("http://localhost:{}", mail.port);

    std::env::set_var("AREEV_TEST_PAIRED_MAIL", "mail-secret");
    let broker = Broker::start(
        // Both destinations are allowed outright — the allowlist is not what
        // is doing the work here.
        policy_for(&[mail_origin.clone(), sheets.origin()]),
        [(
            "gmail".to_string(),
            Credential::bearer_from_env("AREEV_TEST_PAIRED_MAIL").unwrap().into(),
        )]
        .into_iter()
        .collect(),
        EgressGrants::new().grant(
            "t",
            CallerGrant::new()
                .credential_for("gmail", vec![AllowedHost::parse_host_pattern("localhost", "t").unwrap()]),
        ),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();

    // The pairing's own host: allowed, and the secret rides.
    let (code, _) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{mail_origin}/m"), "method": "GET", "credential": "gmail" }),
    );
    assert_eq!(code, 200);
    assert_eq!(
        mail.requests()[0].2.as_deref(),
        Some("bearer mail-secret"),
        "the paired host still gets the credential"
    );

    // The other service: refused, and NOT sent unauthenticated either — the
    // request must not reach it at all.
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/s", sheets.origin()), "method": "GET", "credential": "gmail" }),
    );
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["code"], json!("RUN-E022"));
    assert!(
        body["error"].as_str().unwrap_or_default().contains("different one"),
        "the message says the grant pairs it elsewhere: {body}"
    );
    assert!(
        sheets.requests().is_empty(),
        "no byte may reach the unpaired host: {:?}",
        sheets.requests()
    );

    // And it is audit evidence, worded apart from a credential that was never
    // granted at all.
    let refusals = broker.refusals();
    assert!(
        refusals.iter().any(|r| r.reason.contains("only for other hosts")),
        "{refusals:?}"
    );
}

/// An UNPAIRED grant keeps meaning what it always meant: any host the rest of
/// the chain permits. Making the pairing available must not silently narrow
/// every deployment that never asked for one.
#[test]
fn an_unpaired_grant_still_reaches_every_allowed_host() {
    let a = Upstream::start(vec![("/a", 200, Vec::new(), "{}")]);
    let b = Upstream::start(vec![("/b", 200, Vec::new(), "{}")]);
    std::env::set_var("AREEV_TEST_UNPAIRED", "shared-secret");
    let broker = Broker::start(
        policy_for(&[a.origin(), b.origin()]),
        [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_UNPAIRED").unwrap().into())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    for (up, path) in [(&a, "/a"), (&b, "/b")] {
        let (code, body) = ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}{path}", up.origin()), "method": "GET", "credential": "api" }),
        );
        assert_eq!(code, 200, "{body}");
        assert_eq!(up.requests()[0].2.as_deref(), Some("bearer shared-secret"));
    }
}

/// The DECLARED half of the same pairing, through the broker: a two-block
/// declaration cannot cross-pair even when the host grant is unpaired and
/// permissive. Both halves have to hold, and this is the one that a tool
/// carries with it.
#[test]
fn a_declared_two_service_tool_cannot_cross_pair_its_credentials() {
    let mail = Upstream::start(vec![("/m", 200, Vec::new(), "{}")]);
    let sheets = Upstream::start(vec![("/s", 200, Vec::new(), "{}")]);
    let mail_origin = format!("http://localhost:{}", mail.port);
    let sheets_origin = format!("http://localhost:{}", sheets.port);

    std::env::set_var("AREEV_TEST_DECL_MAIL", "mail-secret");
    std::env::set_var("AREEV_TEST_DECL_SHEETS", "sheets-secret");
    let broker = Broker::start(
        policy_for(&[mail_origin.clone(), sheets_origin.clone()]),
        [
            (
                "gmail".to_string(),
                Credential::bearer_from_env("AREEV_TEST_DECL_MAIL").unwrap().into(),
            ),
            (
                "sheets".to_string(),
                Credential::bearer_from_env("AREEV_TEST_DECL_SHEETS").unwrap().into(),
            ),
        ]
        .into_iter()
        .collect(),
        // Deliberately UNPAIRED and holding both: the grant is not what
        // refuses here.
        EgressGrants::new().grant(
            "t",
            CallerGrant::new().credential("gmail").credential("sheets").method("GET"),
        ),
        "RUN-E022",
    )
    .unwrap();
    broker.declare(
        "t",
        declaration(json!([
            {"http": {"hosts": [mail_origin], "credentials": ["gmail"]}},
            {"http": {"hosts": [sheets_origin], "credentials": ["sheets"]}}
        ])),
        CapabilityLimits::default(),
    );
    let token = broker.token_for("t").unwrap().to_string();

    let (code, body) = ask(
        broker.url(),
        &token,
        json!({
            "url": format!("http://localhost:{}/s", sheets.port),
            "method": "GET",
            "credential": "gmail"
        }),
    );
    assert_eq!(code, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("no single capability pairs them"),
        "the declared half refuses it by name: {body}"
    );
    assert!(sheets.requests().is_empty());
    // Its own credential still reaches its own service.
    let (code, _) = ask(
        broker.url(),
        &token,
        json!({
            "url": format!("http://localhost:{}/m", mail.port),
            "method": "GET",
            "credential": "gmail"
        }),
    );
    assert_eq!(code, 200);
}

// ---- #113: a credential can be MINTED per call, not only read once ---------

/// A resolver command whose stdout becomes the credential. The guest still
/// names a label and holds nothing — only where the value came from changed.
#[cfg(unix)]
#[test]
fn a_command_sourced_credential_is_minted_and_attached() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{\"ok\":true}")]);
    let (source, owner) = CredentialSource::from_spec("cmd:printf minted-token").unwrap();
    assert!(owner.is_none(), "a cmd: spec parses no principal out of its command");

    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), source)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(site.requests()[0].2.as_deref(), Some("bearer minted-token"));
}

/// A counter-file resolver, so "how many times was it minted" is observable.
#[cfg(unix)]
fn counting_resolver(tag: &str) -> (String, std::path::PathBuf) {
    let counter = std::env::temp_dir().join(format!("areev-mint-{tag}.count"));
    let _ = std::fs::remove_file(&counter);
    let c = counter.display();
    (
        format!(
            "cmd:n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); printf %s \"$n\" > {c}; \
             printf 'tok%s' \"$n\""
        ),
        counter,
    )
}

/// Resolving per HTTP call would fork a process per call, so a minted value is
/// cached for its TTL — and re-minted once the TTL lapses, which is what lets
/// a revocation upstream take effect without restarting anything.
#[cfg(unix)]
#[test]
fn a_minted_credential_is_cached_for_its_ttl_and_reminted_after_it() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    let (spec, counter) = counting_resolver("ttl");

    let cached = CredentialSource::from_spec(&spec).unwrap().0.with_resolver_config(Some(300), &[]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), cached)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    for _ in 0..3 {
        let (code, _) = ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
        );
        assert_eq!(code, 200);
    }
    let seen: Vec<_> = site.requests().iter().map(|r| r.2.clone()).collect();
    assert_eq!(
        seen,
        vec![Some("bearer tok1".into()); 3],
        "one mint served all three calls: {seen:?}"
    );
    drop(broker);

    // A zero TTL is the same code path with the window closed: every call
    // mints again.
    let _ = std::fs::remove_file(&counter);
    let fresh = CredentialSource::from_spec(&spec).unwrap().0.with_resolver_config(Some(0), &[]);
    let site2 = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    let broker = Broker::start(
        policy_for(&[site2.origin()]),
        [("api".to_string(), fresh)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    for _ in 0..2 {
        ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}/x", site2.origin()), "method": "GET", "credential": "api" }),
        );
    }
    let seen: Vec<_> = site2.requests().iter().map(|r| r.2.clone()).collect();
    assert_eq!(seen, vec![Some("bearer tok1".into()), Some("bearer tok2".into())], "{seen:?}");
    let _ = std::fs::remove_file(&counter);
}

/// Fail CLOSED. A resolver that exits non-zero must refuse the call, not send
/// it unauthenticated — the unauthenticated version surfaces hours later as a
/// 401 from someone else's API, which is the failure this feature removes.
///
/// And the error names the credential WITHOUT quoting the resolver: stdout is
/// by definition the secret, and stderr is written by a script that may have
/// echoed it.
#[cfg(unix)]
#[test]
fn a_failing_resolver_refuses_the_call_rather_than_sending_it_unauthenticated() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    let source = CredentialSource::from_spec("cmd:echo leaked-secret >&2; exit 7").unwrap().0;
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), source)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["code"], json!("RUN-E022"));
    let text = body["error"].as_str().unwrap_or_default();
    assert!(text.contains("api"), "it names which credential failed: {text}");
    assert!(
        !text.contains("leaked-secret"),
        "and never repeats the resolver's own output: {text}"
    );
    assert!(site.requests().is_empty(), "nothing may be sent unauthenticated");
    // The operator's journal carries WHY; the resolver's own output still
    // reaches neither side.
    let refusals = broker.refusals();
    assert!(
        refusals.iter().any(|r| r.reason.contains("api") && r.reason.contains("resolver exited")),
        "{refusals:?}"
    );
    assert!(refusals.iter().all(|r| !r.reason.contains("leaked-secret")), "{refusals:?}");
}

/// A resolver is an input like any other. One that returns CR/LF would author
/// a second header on every request its credential rides — header injection
/// sourced from the one place this subsystem would otherwise trust.
#[cfg(unix)]
#[test]
fn a_resolver_returning_a_control_character_or_nothing_is_refused() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    for (spec, why) in [
        ("cmd:printf 'tok\\r\\nX-Injected: 1'", "control characters"),
        ("cmd:true", "empty output"),
    ] {
        let source = CredentialSource::from_spec(spec).unwrap().0;
        let broker = Broker::start(
            policy_for(&[site.origin()]),
            [("api".to_string(), source)].into_iter().collect(),
            EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
            "RUN-E022",
        )
        .unwrap();
        let token = broker.token_for("t").unwrap().to_string();
        let (code, body) = ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
        );
        assert_eq!(code, 403, "{why} must be refused: {body}");
    }
    assert!(site.requests().is_empty());
}

/// The expiry case the seam exists for: a cached token lapses upstream before
/// its TTL lapses here. A GET is idempotent, so the broker re-mints and
/// re-issues exactly once rather than handing back a 401 nobody can explain.
#[cfg(unix)]
#[test]
fn an_unauthorized_get_remints_once_and_retries() {
    let site = Upstream::start(vec![("/x", 401, Vec::new(), "expired")]);
    let (spec, counter) = counting_resolver("401get");
    let source = CredentialSource::from_spec(&spec).unwrap().0.with_resolver_config(Some(300), &[]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), source)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    // The upstream 401s whatever we send, so the caller still learns that —
    // the broker brokered the call successfully and reports the upstream's own
    // status in the payload. The point is that a FRESH credential was tried
    // before giving up.
    assert_eq!(code, 200, "the broker answered");
    assert_eq!(body["status"], json!(401), "and relays the upstream's status: {body}");
    let seen: Vec<_> = site.requests().iter().map(|r| r.2.clone()).collect();
    assert_eq!(
        seen,
        vec![Some("bearer tok1".into()), Some("bearer tok2".into())],
        "one retry, with a newly minted credential: {seen:?}"
    );

    // BOTH attempts are journaled. A request that went out carrying a
    // credential is an effect whether or not its response reached the caller,
    // and an audit trail claiming one call where two happened is a false
    // record.
    let calls = broker.calls();
    assert_eq!(calls.len(), 2, "the discarded attempt is evidence too: {calls:?}");
    assert!(calls.iter().all(|c| c.status == 401 && c.credential.as_deref() == Some("api")));
    let _ = std::fs::remove_file(&counter);
}

/// …and NOT for a write. A POST that 401'd may still have been applied
/// upstream, and the broker is not entitled to guess. The stale value is still
/// invalidated, so the caller's own retry gets a fresh one.
#[cfg(unix)]
#[test]
fn an_unauthorized_post_is_not_replayed_but_the_credential_is_still_invalidated() {
    let site = Upstream::start(vec![("/w", 401, Vec::new(), "expired")]);
    let (spec, counter) = counting_resolver("401post");
    let source = CredentialSource::from_spec(&spec).unwrap().0.with_resolver_config(Some(300), &[]);
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), source)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api").method("POST")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    let call = || {
        ask(
            broker.url(),
            &token,
            json!({
                "url": format!("{}/w", site.origin()),
                "method": "POST",
                "body": "{}",
                "credential": "api"
            }),
        )
    };
    call();
    assert_eq!(site.requests().len(), 1, "a write is never replayed by the broker");
    // The caller's own retry gets a freshly minted credential, because the
    // 401 invalidated the cached one even though it did not re-issue.
    call();
    let seen: Vec<_> = site.requests().iter().map(|r| r.2.clone()).collect();
    assert_eq!(
        seen,
        vec![Some("bearer tok1".into()), Some("bearer tok2".into())],
        "{seen:?}"
    );
    let _ = std::fs::remove_file(&counter);
}

/// The environment carve-out (#113). A resolver's own authentication —
/// `VAULT_TOKEN` and friends — is a secret one class more powerful than the
/// credential it fetches, because it can fetch all of them. It reaches the
/// RESOLVER and nothing else: the resolver runs under `ClearExcept`, so an
/// ambient variable is invisible to it unless the operator named it.
#[cfg(unix)]
#[test]
fn a_resolver_sees_only_the_variables_the_operator_named() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    std::env::set_var("AREEV_TEST_RESOLVER_AUTH", "vault-master-token");
    let spec = "cmd:printf %s \"${AREEV_TEST_RESOLVER_AUTH:-absent}\"";

    // Not named: the resolver cannot see it, and says so.
    let blind = CredentialSource::from_spec(spec).unwrap().0;
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), blind)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(site.requests()[0].2.as_deref(), Some("bearer absent"));
    drop(broker);

    // Named via --resolver-env: it reaches the resolver, and only it.
    let site2 = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    let allowed = CredentialSource::from_spec(spec)
        .unwrap()
        .0
        .with_resolver_config(None, &["AREEV_TEST_RESOLVER_AUTH".to_string()]);
    let broker = Broker::start(
        policy_for(&[site2.origin()]),
        [("api".to_string(), allowed)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site2.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(site2.requests()[0].2.as_deref(), Some("bearer vault-master-token"));
    std::env::remove_var("AREEV_TEST_RESOLVER_AUTH");
}

/// A resolved credential must not be printable. A derived `Debug` puts the
/// secret into any error chain or `{:?}` a host reaches for while debugging,
/// which is how one ends up in a log file that outlives the process.
#[test]
fn a_credential_never_debug_prints_its_value() {
    std::env::set_var("AREEV_TEST_DEBUG_REDACT", "hunter2");
    let c = Credential::bearer_from_env("AREEV_TEST_DEBUG_REDACT").unwrap();
    let printed = format!("{c:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(printed.contains("redacted"), "{printed}");

    let h = Credential::Header { name: "X-Api-Key".into(), value: "hunter2".into() };
    let printed = format!("{h:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(printed.contains("X-Api-Key"), "the NAME survives — it is what a reader needs");
    std::env::remove_var("AREEV_TEST_DEBUG_REDACT");
}

/// The spec grammar, including the one asymmetry worth pinning: a command may
/// contain '@', so a `cmd:` source never parses a principal out of it — the
/// name side is where a principal goes for those.
#[test]
fn the_credential_spec_grammar_parses_three_sources() {
    std::env::set_var("AREEV_TEST_SPEC_VAR", "v");
    let (s, owner) = CredentialSource::from_spec("AREEV_TEST_SPEC_VAR@user:alice").unwrap();
    assert!(matches!(s, CredentialSource::Static(_)));
    assert_eq!(owner.as_deref(), Some("user:alice"), "the env form still binds an owner");

    let (s, owner) = CredentialSource::from_spec("cmd:curl -u svc@example.com https://x").unwrap();
    assert!(matches!(s, CredentialSource::Command { .. }));
    assert!(owner.is_none(), "an '@' inside a command is part of the command");

    let (s, owner) = CredentialSource::from_spec("vault:secret/data/google#access_token").unwrap();
    match s {
        CredentialSource::Vault { path, field, .. } => {
            assert_eq!(path, "secret/data/google");
            assert_eq!(field, "access_token");
        }
        other => panic!("{other:?}"),
    }
    assert!(owner.is_none());

    for bad in ["cmd:", "cmd:   ", "vault:secret/data/google", "vault:#field", "vault:path#"] {
        assert!(CredentialSource::from_spec(bad).is_err(), "{bad:?} must not parse");
    }
    std::env::remove_var("AREEV_TEST_SPEC_VAR");
}

/// `VAULT_ADDR`/`VAULT_TOKEN` are process-global, so the tests that set them
/// must not run at the same moment — one would read the other's address and
/// resolve against the wrong server.
static VAULT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The native `vault:` reader, against a stand-in for Vault's KV API.
///
/// Worth testing rather than trusting because it is the one resolver Areev
/// implements itself instead of shelling out: the token must ride the
/// `X-Vault-Token` header, both KV shapes must be read from the operator's
/// verbatim path, and a failure must name the path without ever repeating the
/// secret.
#[test]
fn a_vault_sourced_credential_reads_both_kv_shapes_and_fails_by_name() {
    let vault = Upstream::start(vec![
        // KV v2 nests the secret under data.data; v1 puts it at data.
        ("/v1/secret/data/g", 200, Vec::new(), r#"{"data":{"data":{"access_token":"v2-minted"}}}"#),
        ("/v1/kv/g", 200, Vec::new(), r#"{"data":{"access_token":"v1-minted"}}"#),
        ("/v1/secret/data/nope", 403, Vec::new(), r#"{"errors":["permission denied"]}"#),
    ]);
    let _serialized = VAULT_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    std::env::set_var("VAULT_ADDR", vault.origin());
    std::env::set_var("VAULT_TOKEN", "root-token");

    let call = |path: &str| {
        let source = CredentialSource::from_spec(&format!("vault:{path}#access_token")).unwrap().0;
        let broker = Broker::start(
            policy_for(&[site.origin()]),
            [("api".to_string(), source)].into_iter().collect(),
            EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
            "RUN-E022",
        )
        .unwrap();
        let token = broker.token_for("t").unwrap().to_string();
        ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
        )
    };

    assert_eq!(call("secret/data/g").0, 200);
    assert_eq!(call("kv/g").0, 200);
    let seen: Vec<_> = site.requests().iter().map(|r| r.2.clone()).collect();
    assert_eq!(seen, vec![Some("bearer v2-minted".into()), Some("bearer v1-minted".into())]);

    // The token authenticates to vault by header, never in the URL.
    let asked = vault.requests();
    assert!(
        asked.iter().all(|r| r
            .3
            .iter()
            .any(|(k, v)| k == "x-vault-token" && v == "root-token")),
        "every vault read presents the token as a header: {asked:?}"
    );

    // A refusal names the path and what to check in the OPERATOR's journal —
    // the guest is told only that its credential is unavailable.
    let (code, body) = call("secret/data/nope");
    assert_eq!(code, 403, "{body}");
    let text = body["error"].as_str().unwrap_or_default();
    assert!(!text.contains("secret/data/nope") && !text.contains("root-token"), "{text}");
    assert!(site.requests().len() == 2, "a failed mint sends nothing");

    std::env::remove_var("VAULT_ADDR");
    std::env::remove_var("VAULT_TOKEN");
}

/// A contradictory grant — the same credential written both paired and
/// unpaired — stays PAIRED whichever order the two arrive in. The alternative
/// is a restriction that silently disappears depending on argument order.
#[test]
fn a_pairing_cannot_be_widened_back_by_an_unpaired_grant() {
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    std::env::set_var("AREEV_TEST_NO_WIDEN", "s");
    let elsewhere = || AllowedHost::parse_host_pattern("elsewhere.example", "t").unwrap();

    for (label, grant) in [
        ("paired then unpaired", CallerGrant::new().credential_for("api", vec![elsewhere()]).credential("api")),
        ("unpaired then paired", CallerGrant::new().credential("api").credential_for("api", vec![elsewhere()])),
    ] {
        let broker = Broker::start(
            policy_for(&[site.origin()]),
            [("api".to_string(), Credential::bearer_from_env("AREEV_TEST_NO_WIDEN").unwrap().into())]
                .into_iter()
                .collect(),
            EgressGrants::new().grant("t", grant),
            "RUN-E022",
        )
        .unwrap();
        let token = broker.token_for("t").unwrap().to_string();
        let (code, body) = ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
        );
        assert_eq!(code, 403, "{label}: the pairing must survive: {body}");
    }
    assert!(site.requests().is_empty());
    std::env::remove_var("AREEV_TEST_NO_WIDEN");
}

/// A 401 from a host the credential never reached must NOT invalidate it.
///
/// The chain leaves its start origin, so `left_origin` drops the credential —
/// a 401 from the redirect target says that host wanted its own auth, not that
/// ours expired. Without the `credential_sent` guard an allowed third-party
/// host reached only by redirect would decide when the cache is flushed, and
/// since the flush repopulates and the next call repeats it, the TTL cache
/// collapses into one resolver subprocess per call.
#[cfg(unix)]
#[test]
fn a_401_from_a_hop_the_credential_never_reached_does_not_remint() {
    let other = Upstream::start(vec![("/y", 401, Vec::new(), "want my own auth")]);
    let start = Upstream::start(vec![]);
    start.add_route(("/go", 302, header("Location", &format!("{}/y", other.origin())), ""));

    let (spec, counter) = counting_resolver("nocrossmint");
    let source = CredentialSource::from_spec(&spec).unwrap().0.with_resolver_config(Some(300), &[]);
    let broker = Broker::start(
        policy_for(&[start.origin(), other.origin()]),
        [("api".to_string(), source)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    for _ in 0..3 {
        ask(
            broker.url(),
            &token,
            json!({ "url": format!("{}/go", start.origin()), "method": "GET", "credential": "api" }),
        );
    }

    // The credential rode only the first hop of each chain, and one mint
    // served all three calls — no re-mint, no cache thrash.
    let seen: Vec<_> = start.requests().iter().map(|r| r.2.clone()).collect();
    assert_eq!(seen, vec![Some("bearer tok1".into()); 3], "{seen:?}");
    assert!(
        other.requests().iter().all(|r| r.2.is_none()),
        "the off-origin hop never sees the credential: {:?}",
        other.requests()
    );
    let _ = std::fs::remove_file(&counter);
}

/// A resolver failure tells the GUEST only that the credential is unavailable,
/// and the operator the rest.
///
/// The detail names infrastructure a capability tool has no business learning
/// — a vault's address, its mount, the secret's path — and that tool may be
/// code that arrived in a synced memory. Same split the principal-binding
/// refusal makes: the journal carries what the operator needs.
#[test]
fn a_resolver_failure_tells_the_guest_nothing_about_the_vault() {
    let _serialized = VAULT_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let site = Upstream::start(vec![("/x", 200, Vec::new(), "{}")]);
    std::env::set_var("VAULT_ADDR", "http://vault.internal.example:8200");
    std::env::set_var("VAULT_TOKEN", "root-token");
    let source = CredentialSource::from_spec("vault:secret/data/payroll#token").unwrap().0;
    let broker = Broker::start(
        policy_for(&[site.origin()]),
        [("api".to_string(), source)].into_iter().collect(),
        EgressGrants::new().grant("t", CallerGrant::new().credential("api")),
        "RUN-E022",
    )
    .unwrap();
    let token = broker.token_for("t").unwrap().to_string();
    let (code, body) = ask(
        broker.url(),
        &token,
        json!({ "url": format!("{}/x", site.origin()), "method": "GET", "credential": "api" }),
    );
    assert_eq!(code, 403, "{body}");
    let text = body["error"].as_str().unwrap_or_default();
    for leaked in ["vault.internal.example", "secret/data/payroll", "root-token"] {
        assert!(!text.contains(leaked), "the guest must not learn {leaked:?}: {text}");
    }
    assert!(text.contains("api") && text.contains("did not resolve"), "{text}");

    // …while the operator's audit trail carries the diagnostic.
    let refusals = broker.refusals();
    assert!(
        refusals.iter().any(|r| r.reason.contains("secret/data/payroll")),
        "the journal keeps what the guest was denied: {refusals:?}"
    );
    assert!(
        refusals.iter().all(|r| !r.reason.contains("root-token")),
        "and never the token itself: {refusals:?}"
    );
    std::env::remove_var("VAULT_ADDR");
    std::env::remove_var("VAULT_TOKEN");
}
