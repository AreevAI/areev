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
