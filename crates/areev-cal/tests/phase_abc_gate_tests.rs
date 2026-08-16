use std::collections::BTreeSet;

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::types::{
    ContentBlock, Event, Grain, Role, TokenUsage, Tool, ToolKind,
};
use areev_store::Areev;

#[test]
fn complete_typed_turn_round_trips_with_identical_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let a_path = dir.path().join("turn-a.db");
    let b_path = dir.path().join("turn-b.db");
    let bundle = dir.path().join("turn.mgb");
    let mut a = Areev::open(a_path.to_str().unwrap()).unwrap();

    let catalog = a
        .add(
            &Tool::new("calculator")
                .kind(ToolKind::Definition)
                .tool_description("Add two numbers")
                .input_schema(serde_json::json!({
                    "type": "object",
                    "properties": {"a":{"type":"number"},"b":{"type":"number"}},
                    "required": ["a", "b"]
                }))
                .namespace("caller")
                .created_at(1),
        )
        .unwrap();
    let system = a
        .add(
            &Event::new("You are precise.")
                .role(Role::System)
                .session("session-1".into())
                .content_blocks(vec![ContentBlock::Text {
                    text: "You are precise.".into(),
                }])
                .namespace("caller")
                .created_at(2),
        )
        .unwrap();
    let user = a
        .add(
            &Event::new("What is 2 + 3?")
                .role(Role::User)
                .session("session-1".into())
                .parent_message(system.to_hex())
                .content_blocks(vec![ContentBlock::Text {
                    text: "What is 2 + 3?".into(),
                }])
                .namespace("caller")
                .created_at(3),
        )
        .unwrap();
    let assistant_call = a
        .add(
            &Event::new("[tool_use calculator]")
                .role(Role::Assistant)
                .session("session-1".into())
                .parent_message(user.to_hex())
                .content_blocks(vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "calculator".into(),
                    input: serde_json::json!({"a":2,"b":3}),
                }])
                .model("model:v1".into())
                .stop_reason("tool_use".into())
                .token_usage(TokenUsage {
                    input_tokens: 12,
                    output_tokens: 4,
                    cache_read_tokens: None,
                    cache_creation_tokens: None,
                })
                .run_id("run-1".into())
                .namespace("caller")
                .created_at(4),
        )
        .unwrap();
    let execution = a
        .add(
            &Tool::new("calculator")
                .input(serde_json::json!({"a":2,"b":3}))
                .content("5")
                .is_error(false)
                .tool_call_id("call-1")
                .namespace("caller")
                .created_at(5),
        )
        .unwrap();
    let tool_result = a
        .add(
            &Event::new("[tool_result] 5")
                .role(Role::Tool)
                .session("session-1".into())
                .parent_message(assistant_call.to_hex())
                .content_blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "5".into(),
                    is_error: Some(false),
                }])
                .run_id("run-1".into())
                .namespace("caller")
                .created_at(6),
        )
        .unwrap();
    let final_output = a
        .add(
            &Event::new("The answer is 5.")
                .role(Role::Assistant)
                .session("session-1".into())
                .parent_message(tool_result.to_hex())
                .content_blocks(vec![ContentBlock::Text {
                    text: "The answer is 5.".into(),
                }])
                .model("model:v1".into())
                .stop_reason("end_turn".into())
                .run_id("run-1".into())
                .namespace("caller")
                .created_at(7),
        )
        .unwrap();

    let facade = AreevFacade::with_session(a, Some("caller".into()), None);
    facade
        .record_run_manifest(
            "run-1",
            r#"{"model":{"base":"model:v1"},"policy":{"version":"p1"},"seed":7}"#,
        )
        .unwrap();
    CalExecutor::new(CalExecutorConfig {
        assembly_manifest_sample_rate: 1.0,
        ..CalExecutorConfig::default()
    })
    .execute(
        r#"ASSEMBLE "turn" FROM transcript: (RECALL events WHERE session_id = "session-1") BUDGET 512 tokens FORMAT json"#,
        &facade,
    )
    .unwrap();
    let mut a = facade.into_inner();

    let original: BTreeSet<String> = a
        .changes_since(0, 10_000)
        .unwrap()
        .into_iter()
        .map(|op| op.hash.to_hex())
        .collect();
    for expected in [
        catalog,
        system,
        user,
        assistant_call,
        execution,
        tool_result,
        final_output,
    ] {
        assert!(original.contains(&expected.to_hex()));
    }
    a.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    let mut b = Areev::open(b_path.to_str().unwrap()).unwrap();
    let first = b.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert!(first.applied > 0);
    let second = b.import_bundle(bundle.to_str().unwrap()).unwrap();
    assert_eq!(second.applied, 0, "re-adding identical content is a no-op");
    for hash in &original {
        let parsed = areev_core::Hash::from_hex(hash).unwrap();
        assert_eq!(b.get(&parsed).unwrap().hash.to_hex(), *hash);
    }
    let call = b.get(&assistant_call).unwrap();
    assert_eq!(call.fields["content_blocks"][0]["input"]["a"], 2);
    assert_eq!(call.fields["model_id"], "model:v1");
}
