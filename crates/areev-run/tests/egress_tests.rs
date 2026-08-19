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
