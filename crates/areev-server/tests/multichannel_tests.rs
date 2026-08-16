//! §8 acceptance: multi-channel agents (voice edge + WhatsApp + email)
//! sharing one user memory through areevd, with a cross-channel conflict
//! surfacing as contested tips instead of silent loss.

use std::io::{Read, Write};
use std::net::TcpStream;

use areev_cal::AreevFacade;
use areev_core::types::{Fact, Grain};
use areev_server::UiServer;
use areev_store::Areev;
use tempfile::TempDir;

const TOKEN: &str = "hub-secret";

fn req(addr: &str, raw: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn post_cal(addr: &str, query: &str) -> String {
    let body = serde_json::json!({ "query": query }).to_string();
    req(addr, &format!(
        "POST /api/cal HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body))
}

fn post_segment(addr: &str, name: &str, bytes: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    let head = format!(
        "POST /api/segment?name={name} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len());
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(bytes).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn fact(ns: &str, s: &str, r: &str, o: &str, at: i64) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f.common.created_at = Some(at);
    f
}

#[test]
fn three_channels_share_one_user_memory() {
    let d = TempDir::new().unwrap();

    // areevd hub owning user:john
    let hub_store = Areev::open(d.path().join("user_john.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(hub_store, Some("caller".into()), None);
    let server = UiServer::new(facade, "user:john".into())
        .into_hub(Some(TOKEN.into()), d.path().join("segments").to_str().unwrap())
        .unwrap();
    let listener = UiServer::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || server.serve(listener));

    // auth is enforced
    let no_auth = req(&addr, "POST /api/cal HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    assert!(no_auth.contains("401"), "{no_auth}");

    // 1) voice edge: embedded writes during a call, then segment push
    let edge_path = d.path().join("edge_john.db");
    let mut edge = Areev::open(edge_path.to_str().unwrap()).unwrap();
    let v1 = edge.add(&fact("caller", "john", "callback_time", "morning", 1_800_000_000_000)).unwrap();
    edge.add(&fact("caller", "john", "prefers", "short calls", 1_800_000_000_001)).unwrap();
    let seg = d.path().join("edge-delta.mgb");
    edge.bundle_since(0, seg.to_str().unwrap()).unwrap();
    let resp = post_segment(&addr, "edge-0001.mgb", &std::fs::read(&seg).unwrap());
    assert!(resp.contains("\"applied\":2"), "{resp}");

    // 2) WhatsApp agent learns over HTTP CAL
    let resp = post_cal(&addr, r#"ADD fact SET subject = "john" SET relation = "whatsapp_optin" SET object = "yes" SET namespace = "caller" REASON "wa""#);
    assert!(resp.contains("\"ok\":true"), "{resp}");

    // 3) email agent recalls — sees facts from BOTH other channels
    let resp = post_cal(&addr, r#"RECALL facts WHERE subject = "john""#);
    assert!(resp.contains("callback_time") && resp.contains("whatsapp_optin"), "{resp}");

    // 4) cross-channel conflict: voice edge supersedes v1 offline → v2a;
    //    WhatsApp supersedes v1 at the hub → v2b; edge delta then lands.
    let mut v2a = fact("caller", "john", "callback_time", "evening", 1_800_000_100_000);
    edge.supersede(&v1, &mut v2a).unwrap();
    let resp = post_cal(&addr, &format!(
        r#"SUPERSEDE sha256:{} SET object = "afternoon" REASON "wa update""#, v1.to_hex()));
    assert!(resp.contains("\"ok\":true"), "{resp}");
    let seg2 = d.path().join("edge-delta2.mgb");
    edge.bundle_since(2, seg2.to_str().unwrap()).unwrap();
    let resp = post_segment(&addr, "edge-0002.mgb", &std::fs::read(&seg2).unwrap());
    assert!(resp.contains("\"ok\":true"), "{resp}");

    // both tips visible to every channel (contested, not lost)
    let resp = post_cal(&addr, r#"RECALL facts WHERE subject = "john" AND relation = "callback_time""#);
    assert!(resp.contains("evening") && resp.contains("afternoon"),
        "fork must surface both versions: {resp}");
}
