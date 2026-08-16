//! Local console API smoke test — real TCP, in-process server thread.

use std::io::{Read, Write};
use std::net::TcpStream;

use areev_cal::AreevFacade;
use areev_server::UiServer;
use areev_store::Areev;
use tempfile::TempDir;

fn req(addr: &str, raw: &str) -> String {
    // The server runs with_auth (§5.7: token-less is read-only), so inject the
    // console token after the request line on every call.
    let raw = match raw.find("\r\n") {
        Some(pos) => format!("{}\r\nAuthorization: Bearer t{}", &raw[..pos], &raw[pos..]),
        None => raw.to_string(),
    };
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn console_api_round_trip() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("ui.db");
    let m = Areev::open(db.to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".into()), None);
    let server = UiServer::new(facade, "ui.db".into()).with_auth("t".to_string());
    let listener = UiServer::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || server.serve(listener));

    // page
    let page = req(&addr, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(page.contains("200 OK") && page.contains("areev"), "{page}");

    // CAL ADD through the API
    let body = r#"{"query":"ADD fact SET subject = \"alice\" SET relation = \"prefers\" SET object = \"tea\" SET namespace = \"caller\" REASON \"ui\""}"#;
    let post = format!(
        "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let resp = req(&addr, &post);
    assert!(resp.contains("\"ok\":true") && resp.contains("hash"), "{resp}");

    // stats reflect the write
    let stats = req(&addr, "GET /api/stats HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(stats.contains("\"grains\":1"), "{stats}");

    // op-log has one add
    let log = req(&addr, "GET /api/log HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(log.contains("\"op\":\"add\""), "{log}");

    // verify endpoint
    let v = req(&addr, "GET /api/verify HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(v.contains("\"integrity\":\"ok\""), "{v}");

    // browse endpoint joins the op-log with grain summaries
    let b = req(&addr, "GET /api/browse HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(
        b.contains("\"ok\":true") && b.contains("\"type\":\"fact\"") && b.contains("\"subject\":\"alice\""),
        "{b}"
    );

    // config endpoint reports the effective per-process configuration,
    // including the file's own declarations (meta table)
    let cfg = req(&addr, "GET /api/config HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(
        cfg.contains("\"tier1_writes\":true")
            && cfg.contains("\"rrf_k0\":60.0")
            && cfg.contains("\"index_text\":true")
            && cfg.contains("\"hub_mode\":false")
            && cfg.contains("\"file\"")
            && cfg.contains("\"warnings\":[]"),
        "{cfg}"
    );

    // cross-origin POSTs are rejected (drive-by protection); loopback
    // origins — the console itself — pass through
    let body = r#"{"query":"RECALL facts WHERE subject = \"alice\""}"#;
    let evil = format!(
        "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let e = req(&addr, &evil);
    assert!(e.contains("403 Forbidden"), "{e}");
    let local = format!(
        "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1:7437\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let l = req(&addr, &local);
    assert!(l.contains("\"ok\":true"), "{l}");

    // CAL errors are structured: code + span for the console's caret
    let bad = r#"{"query":"RECALL facts WHERE subject == \"alice\""}"#;
    let post = format!(
        "POST /api/cal HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        bad.len(),
        bad
    );
    let e = req(&addr, &post);
    assert!(
        e.contains("\"ok\":false") && e.contains("\"code\":\"CAL-") && e.contains("\"span\""),
        "{e}"
    );
}

/// A host loop policy attached at server start (`areev ui --policy`) governs
/// console-triggered runs: the granted structural consolidation auto-applies,
/// exactly as it would under `areev loop run --policy`.
#[test]
fn console_run_honors_attached_loop_policy() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("pol.db");
    let mut m = Areev::open(db.to_str().unwrap()).unwrap();
    // A case-variant exact duplicate (distinct bytes, same normalized value).
    use areev_core::types::{Fact, Grain};
    m.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller")).unwrap();
    m.add(&Fact::new("acme", "tier", "enterprise").namespace("caller")).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".into()), None);
    let policy = areev_loop::Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let server = UiServer::new(facade, "pol.db".into())
        .with_auth("t".to_string())
        .with_loop_policy(policy);
    let listener = UiServer::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || server.serve(listener));

    let post = "POST /api/loop/run HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let resp = req(&addr, post);
    assert!(resp.contains("\"auto_applied\":1"), "{resp}");
}

/// With `with_auth`, every request needs the token. Browsers authenticate via
/// the native HTTP Basic prompt (any username, password = token); scripts via
/// `Authorization: Bearer`. Missing/wrong credentials get a 401 that carries a
/// `WWW-Authenticate: Basic` challenge so the browser prompts.
#[test]
fn console_auth_guards_every_request() {
    use std::io::Read as _;
    // base64("x:opensesame") — any username, password = the token.
    const BASIC_OK: &str = "eDpvcGVuc2VzYW1l";
    // base64("x:wrong")
    const BASIC_BAD: &str = "eDp3cm9uZw==";

    let dir = TempDir::new().unwrap();
    let db = dir.path().join("auth.db");
    let m = Areev::open(db.to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".into()), None);
    let server = UiServer::new(facade, "auth.db".into()).with_auth("opensesame".into());
    let listener = UiServer::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || server.serve(listener));

    let status = |raw: &str| -> String {
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out.lines().next().unwrap_or("").to_string()
    };
    let g = |auth: &str| {
        status(&format!(
            "GET /api/config HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"
        ))
    };

    // Even a plain read is guarded (the console page and reads, not just POSTs).
    let no_auth = req(&addr, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(no_auth.contains("401"), "page must be guarded: {no_auth}");
    assert!(
        no_auth.contains("WWW-Authenticate: Basic"),
        "401 must challenge so browsers prompt: {no_auth}"
    );

    assert!(g("").contains("401"), "no creds → 401");
    assert!(g("Authorization: Bearer opensesame\r\n").contains("200"), "bearer → 200");
    assert!(g(&format!("Authorization: Basic {BASIC_OK}\r\n")).contains("200"), "basic → 200");
    assert!(g(&format!("Authorization: Basic {BASIC_BAD}\r\n")).contains("401"), "wrong basic → 401");
    assert!(g("Authorization: Bearer nope\r\n").contains("401"), "wrong bearer → 401");
}

/// DNS-rebinding defense: the default (loopback) console rejects any request
/// whose `Host` header is not loopback — this is what stops a drive-by web
/// page (which rebinds its own domain to 127.0.0.1) from reading memory over
/// GET, where the Origin check does not apply. `allow_remote(true)` opts out.
#[test]
fn non_loopback_host_is_rejected_unless_allow_remote() {
    fn spawn(server: UiServer) -> String {
        let listener = UiServer::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || server.serve(listener));
        addr
    }
    let mk = || {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("ui.db");
        let m = Areev::open(db.to_str().unwrap()).unwrap();
        // Keep the TempDir alive for the server thread's lifetime.
        std::mem::forget(dir);
        AreevFacade::with_session(m, Some("caller".into()), None)
    };

    // Default console: a rebinding GET (attacker domain in Host) is refused,
    // and a real loopback GET still works.
    let addr = spawn(UiServer::new(mk(), "ui.db".into()));
    let evil = req(&addr, "GET /api/browse HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n");
    assert!(evil.contains("403") && evil.contains("non-loopback Host"), "rebinding GET must be 403: {evil}");
    let ok = req(&addr, "GET /api/browse HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    assert!(ok.contains("200"), "loopback GET must pass: {ok}");

    // With --allow-remote, a non-loopback Host is accepted (operator opted in).
    let addr = spawn(UiServer::new(mk(), "ui.db".into()).allow_remote(true));
    let remote = req(&addr, "GET /api/browse HTTP/1.1\r\nHost: memories.example\r\nConnection: close\r\n\r\n");
    assert!(remote.contains("200"), "allow_remote must accept non-loopback Host: {remote}");
}
