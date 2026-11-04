//! Dual-control reveal (#417): a requester asks with a reason, a DIFFERENT
//! admin approves, and the requester redeems once within the TTL. Every
//! step and every refusal is a Tier-2 record naming both principals.

use areev_cal::facade::CalStoreFacade;
use areev_cal::AreevFacade;
use areev_core::authz::{Grant, Verb};
use areev_store::Areev;
use tempfile::TempDir;

const T0: i64 = 1_800_000_000_000;

/// An encrypted memory with a vault-backed policy on `caller`, one fact
/// about `caller:john`, the placeholder that recall minted for him, and
/// three principals: `user:a` and `user:b` (admin on caller), `user:r`
/// (read only).
fn setup(policy: &str) -> (AreevFacade, String, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open_encrypted(dir.path().join("v.db").to_str().unwrap(), [7u8; 32]).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    f.with_store(|m| m.set_anon_policy("caller", policy)).unwrap();
    let mut fields = serde_json::Map::new();
    fields.insert("subject".into(), serde_json::json!("caller:john"));
    fields.insert("relation".into(), serde_json::json!("prefers"));
    fields.insert("object".into(), serde_json::json!("tea"));
    fields.insert("namespace".into(), serde_json::json!("caller"));
    f.cal_add_if_novel("fact", &fields).unwrap();
    let mut params = areev_cal::store_types::RecallParams::default();
    params.subject = Some("caller:john".to_string());
    params.namespace = Some("caller".to_string());
    params.limit = Some(4);
    let hits = f.recall(&params).unwrap();
    let token = hits[0].grain.fields["subject"].as_str().unwrap().to_string();
    assert!(token.starts_with('['), "recall must pseudonymize: {token}");
    let admin = Grant { verbs: vec![Verb::Admin, Verb::Read], namespaces: vec!["caller".into()] };
    let read = Grant { verbs: vec![Verb::Read], namespaces: vec!["caller".into()] };
    f.set_grants("user:a", std::slice::from_ref(&admin), "safety desk").unwrap();
    f.set_grants("user:b", &[admin], "approver").unwrap();
    f.set_grants("user:r", &[read], "reader").unwrap();
    (f, token, dir)
}

const DUAL: &str =
    r#"{"mode": "egress", "scope": "session", "vault": true, "reveal": {"mode": "dual", "ttl_secs": 1800}}"#;

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

fn code(e: areev_core::error::AreevError) -> String {
    e.code().to_string()
}

/// The reveal trail, oldest first: (verb, principal, context.reveal).
fn trail(f: &AreevFacade) -> Vec<(String, String, serde_json::Value)> {
    let mut rows: Vec<(i64, String, String, serde_json::Value)> = f
        .with_store(|m| m.recent("agent:authz", Some(areev_core::types::GrainType::Observation), 200))
        .unwrap()
        .into_iter()
        .filter_map(|g| {
            let ctx = g.fields.get("context")?.clone();
            let verb = ctx.get("verb")?.as_str()?.to_string();
            if !verb.starts_with("reveal") {
                return None;
            }
            let seq = ctx.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
            let who = g.fields.get("observer_id")?.as_str()?.to_string();
            Some((seq, verb, who, ctx.get("reveal").cloned().unwrap_or_default()))
        })
        .collect();
    rows.sort_by_key(|r| r.0);
    rows.into_iter().map(|(_, v, w, r)| (v, w, r)).collect()
}

#[test]
fn request_approve_reveal_is_one_chain_naming_both_principals() {
    let (f, token, _d) = setup(DUAL);

    f.bind_principal("user:a").unwrap();
    // A direct reveal is refused under dual control, and the refusal is recorded.
    let err = f.reveal_tokens("caller", std::slice::from_ref(&token)).unwrap_err();
    assert_eq!(code(err), "AUT-E005");
    // A reason is mandatory.
    assert!(f.reveal_request_at("caller", std::slice::from_ref(&token), "  ", T0).is_err());
    let req = json(
        &f.reveal_request_at("caller", std::slice::from_ref(&token), "safety escalation #12", T0)
            .unwrap(),
    );
    let id = req["request_id"].as_str().unwrap().to_string();
    assert_eq!(req["requester"], "user:a");
    assert_eq!(req["expires_at"], T0 + 1_800_000);

    // Self-approval is refused.
    assert_eq!(code(f.reveal_approve_at(&id, T0 + 1).unwrap_err()), "AUT-E005");
    // Redeeming before approval is refused.
    assert_eq!(code(f.reveal_redeem_at(&id, T0 + 2).unwrap_err()), "AUT-E005");

    // A reader cannot approve: that is a missing grant, not a dual refusal.
    f.bind_principal("user:r").unwrap();
    assert_eq!(code(f.reveal_approve_at(&id, T0 + 3).unwrap_err()), "AUT-E001");

    f.bind_principal("user:b").unwrap();
    let ok = json(&f.reveal_approve_at(&id, T0 + 10_000).unwrap());
    assert_eq!(ok["approver"], "user:b");
    assert_eq!(ok["because"], "safety escalation #12");
    // The approver cannot redeem someone else's request.
    assert_eq!(code(f.reveal_redeem_at(&id, T0 + 10_001).unwrap_err()), "AUT-E005");
    // A second approval is refused.
    assert_eq!(code(f.reveal_approve_at(&id, T0 + 10_002).unwrap_err()), "AUT-E005");

    f.bind_principal("user:a").unwrap();
    let out = json(&f.reveal_redeem_at(&id, T0 + 20_000).unwrap());
    assert_eq!(out["revealed"][&token], "caller:john");
    // Single use.
    assert_eq!(code(f.reveal_redeem_at(&id, T0 + 20_001).unwrap_err()), "AUT-E005");

    let t = trail(&f);
    let verbs: Vec<&str> = t.iter().map(|(v, _, _)| v.as_str()).collect();
    assert_eq!(
        verbs,
        [
            "reveal.refused", // direct reveal under dual
            "reveal.request",
            "reveal.refused", // self-approval
            "reveal.refused", // redeem before approval
            "reveal.approve",
            "reveal.refused", // approver redeeming
            "reveal.refused", // second approval
            "reveal",
            "reveal.refused", // second use
        ]
    );
    let request = &t[1];
    let approve = &t[4];
    let reveal = &t[7];
    assert_eq!(request.1, "user:a");
    assert_eq!(approve.1, "user:b");
    assert_eq!(approve.2["request_id"], id.as_str());
    assert_eq!(approve.2["requester"], "user:a");
    assert_eq!(reveal.1, "user:a");
    assert_eq!(reveal.2["request_id"], id.as_str());
    assert_eq!(reveal.2["requester"], "user:a");
    assert_eq!(reveal.2["approver"], "user:b");
    assert_eq!(t[2].2["reason"], "self_approval");
    assert_eq!(t[8].2["reason"], "already_used");
    // The request id IS the request record's content address.
    let records = f
        .with_store(|m| m.recent("agent:authz", Some(areev_core::types::GrainType::Observation), 200))
        .unwrap();
    assert!(records.iter().any(|g| g.hash.to_hex() == id));
    // No record carries the identity.
    for g in &records {
        let s = serde_json::to_string(&g.fields).unwrap();
        assert!(!s.contains("caller:john"), "an audit grain re-identifies: {s}");
    }
}

#[test]
fn the_ttl_bounds_approval_and_redemption() {
    let (f, token, _d) = setup(DUAL);
    let ttl = 1_800_000;

    // Approval after the request's TTL is refused.
    f.bind_principal("user:a").unwrap();
    let id = json(&f.reveal_request_at("caller", std::slice::from_ref(&token), "why", T0).unwrap())
        ["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.bind_principal("user:b").unwrap();
    assert_eq!(code(f.reveal_approve_at(&id, T0 + ttl + 1).unwrap_err()), "AUT-E005");

    // Redemption after the approval's TTL is refused; the window restarts at
    // approval, so a late approval still gets the full window.
    f.bind_principal("user:a").unwrap();
    let id = json(&f.reveal_request_at("caller", std::slice::from_ref(&token), "why", T0).unwrap())
        ["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.bind_principal("user:b").unwrap();
    f.reveal_approve_at(&id, T0 + ttl - 1).unwrap();
    f.bind_principal("user:a").unwrap();
    assert_eq!(code(f.reveal_redeem_at(&id, T0 + 2 * ttl).unwrap_err()), "AUT-E005");
    let t = trail(&f);
    assert_eq!(t.last().unwrap().2["reason"], "expired");
}

#[test]
fn single_mode_keeps_the_direct_reveal_and_also_accepts_requests() {
    let (f, token, _d) =
        setup(r#"{"mode": "egress", "scope": "session", "vault": true}"#);
    f.bind_principal("user:a").unwrap();
    let out = json(&f.reveal_tokens("caller", std::slice::from_ref(&token)).unwrap());
    assert_eq!(out["revealed"][&token], "caller:john");
    // The request flow works under single too (it only ever adds a person).
    let req = json(&f.reveal_request_at("caller", std::slice::from_ref(&token), "why", T0).unwrap());
    assert_eq!(req["mode"], "single");
    // An unknown request id is AUT-E005.
    assert_eq!(code(f.reveal_redeem("deadbeef").unwrap_err()), "AUT-E005");
}

#[test]
fn request_state_is_durable_across_reopen() {
    let (f, token, d) = setup(DUAL);
    f.bind_principal("user:a").unwrap();
    let id = json(&f.reveal_request_at("caller", std::slice::from_ref(&token), "why", T0).unwrap())
        ["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(f);
    // Reopen the same file: the pending request is still there, and the
    // approval/redemption run on the new handle.
    let m = Areev::open_encrypted(d.path().join("v.db").to_str().unwrap(), [7u8; 32]).unwrap();
    let row = m.meta_get(&format!("{}{id}", areev_store::REVEAL_REQUEST_PREFIX)).unwrap();
    assert!(row.unwrap().contains("\"state\":\"pending\""));
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    f.bind_principal("user:b").unwrap();
    f.reveal_approve_at(&id, T0 + 1).unwrap();
    f.bind_principal("user:a").unwrap();
    // The vault survives the reopen too, so the redemption recovers the value.
    let out = json(&f.reveal_redeem_at(&id, T0 + 2).unwrap());
    assert_eq!(out["revealed"][&token], "caller:john");
}
