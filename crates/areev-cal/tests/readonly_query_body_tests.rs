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
