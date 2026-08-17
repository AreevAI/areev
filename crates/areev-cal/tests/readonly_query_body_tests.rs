//! Regression: saved-query bodies stay read-only (2026-07-22 hunt, #14).
//! The DEFINE-time scan for $-parameterized bodies previously missed
//! DROP/DEFINE and was whitespace-evadable; RUN never re-validated.
use areev_cal::parser::parse;

#[test]
fn param_body_rejects_writes_and_recursion() {
    // DROP was omitted from the old scan list.
    assert!(parse(r#"DEFINE QUERY "e1" ($n) AS { DROP TEMPLATE $n }"#).is_err(), "DROP must be refused");
    // DEFINE was omitted too.
    assert!(parse(r#"DEFINE QUERY "e2" ($n) AS { DEFINE TEMPLATE $n AS "x" }"#).is_err(), "DEFINE must be refused");
    // Whitespace evasion: newline after SUPERSEDE dodged the "SUPERSEDE " substring test.
    assert!(
        parse("DEFINE QUERY \"e3\" ($v) AS { SUPERSEDE\nsha256:aaaa SET object = $v REASON \"x\" }").is_err(),
        "SUPERSEDE with a newline must be refused"
    );
    assert!(parse(r#"DEFINE QUERY "e4" ($h) AS { FORGET $h }"#).is_err(), "FORGET must be refused");
    assert!(parse(r#"DEFINE QUERY "e5" ($n) AS { RUN $n }"#).is_err(), "RUN recursion must be refused");
}

#[test]
fn read_only_param_body_still_accepted() {
    // A genuine read-only parameterized body must still DEFINE fine.
    assert!(parse(r#"DEFINE QUERY "ok1" ($s) AS { RECALL facts WHERE subject = $s }"#).is_ok());
    // No-parameter bodies still take the precise parse-time path.
    assert!(parse(r#"DEFINE QUERY "ok2" AS { RECALL facts WHERE subject = "john" }"#).is_ok());
    assert!(parse(r#"DEFINE QUERY "bad" AS { DROP TEMPLATE "x" }"#).is_err());
}

// ── Issue #24: a parameterized body that can never RUN ─────────────────────
// The DEFINE-time check used to stop at the keyword scan above whenever the
// body contained `$`, so a body with a genuine syntax error was stored and only
// exploded on the first caller — which is typically an unattended agent, long
// after whoever wrote the query left. The body is now parsed at DEFINE.

#[test]
fn param_body_with_a_syntax_error_is_refused_at_define() {
    // The reported shape: THREAD is a lexer token no production consumes, so
    // this body cannot parse at DEFINE or at RUN.
    let e = parse(
        r#"DEFINE QUERY "triage" ($session) AS { ASSEMBLE "t" FROM thread: (RECALL events THREAD $session) BUDGET 4000 tokens FORMAT sml }"#,
    )
    .expect_err("a body that can never RUN must not be stored");
    assert!(e.to_string().starts_with("CAL-E059"), "got {e}");

    // Trailing garbage after an otherwise valid statement.
    assert!(
        parse(r#"DEFINE QUERY "g" ($s) AS { RECALL facts WHERE subject = $s THIS IS GARBAGE }"#)
            .is_err(),
        "trailing garbage in a parameterized body must be refused"
    );
}

#[test]
fn params_in_positions_that_demand_a_literal_still_define() {
    // Why the parse used to be skipped: a parameter in a NUMERIC position is
    // not a valid number literal until RUN substitutes it. These must NOT
    // regress into false rejections — that would break every saved query that
    // parameterizes a limit.
    assert!(parse(r#"DEFINE QUERY "n1" ($limit) AS { RECALL facts WHERE subject = "a" RECENT $limit }"#).is_ok());
    assert!(parse(r#"DEFINE QUERY "n2" ($k) AS { RECALL facts WHERE subject = "a" LIMIT $k }"#).is_ok());
    // Value positions were always parseable and must stay accepted.
    assert!(parse(r#"DEFINE QUERY "v1" ($s) AS { RECALL events WHERE session_id = $s RECENT 10 }"#).is_ok());
    // Multi-source ASSEMBLE with a parameterized source — the shape the report
    // was actually reaching for.
    assert!(parse(
        r#"DEFINE QUERY "a1" ($s) AS { ASSEMBLE "t" FROM thread: (RECALL events WHERE session_id = $s RECENT 10) BUDGET 4000 tokens FORMAT sml }"#
    )
    .is_ok());
}

#[test]
fn a_dollar_inside_a_string_literal_is_data_not_a_parameter() {
    // The placeholder rewrite the shape check uses must step over string
    // literals, or a perfectly valid body gets refused for containing money.
    assert!(parse(
        r#"DEFINE QUERY "money" ($s) AS { RECALL facts WHERE subject = $s AND object = "costs $5.00" }"#
    )
    .is_ok());
}
