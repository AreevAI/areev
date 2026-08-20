//! The CAL error contract: what a malformed query is *told*.
//!
//! Two invariants meet here, and neither had a test that walked the surface:
//!
//! 1. **Every user-facing error leads with its `DOMAIN-Ennn` code** — the code
//!    is the first token of the `Display` string, so a bug report that quotes
//!    only the message still locates the variant.
//! 2. **Codes are append-only** — `ERROR_CODES.md` registers the CAL domain as
//!    *ranges* (the per-variant source of truth is the inline docs on
//!    `CalError`), so the contract is that no emitted code escapes the table.
//!    A code callers can receive but the registry never accounted for is a
//!    broken contract for anyone matching on it.
//!
//! The table below is written as *repros*, not as constructed enum variants:
//! each row is a query a user could actually type, so the test also pins the
//! rejection itself. A query that silently starts parsing — `DELETE`, `DROP
//! TABLE` — would fail here as loudly as a wrong code.

use areev_cal::parse;

/// (query, expected code, what a user did wrong)
const REPROS: &[(&str, &str, &str)] = &[
    ("", "CAL-E014", "an empty query"),
    ("RECALL", "CAL-E002", "a verb with no grain type"),
    ("RECALL widgets", "CAL-E003", "a grain type that does not exist"),
    (
        "RECALL facts WHERE subject = \"unterminated",
        "CAL-E005",
        "a string that never closes",
    ),
    ("RECALL facts LIMIT 999999999", "CAL-E010", "a limit past the cap"),
    ("RECALL facts EXTRAGARBAGE", "CAL-E002", "trailing tokens after a valid query"),
    (
        "RECALL facts WHERE subject = \"a\" | WHERE relation = \"b\"",
        "CAL-E002",
        "a stage that is not a pipeline stage",
    ),
    ("RECALL facts WHERE subject = 1.2.3", "CAL-E002", "a malformed number"),
    ("PURGE OLDER THAN 5d", "CAL-E018", "a retention sweep with no recorded reason"),
    // The three-layer destructive block: these are not verbs in this grammar,
    // and they must be rejected at the lexer rather than parsed and refused
    // later. A regression here is a security regression, not a UX one.
    ("DELETE sha256:abc", "CAL-E002", "DELETE is not a token in CAL"),
    ("ERASE facts", "CAL-E002", "ERASE is not a token in CAL"),
    ("TRUNCATE facts", "CAL-E002", "TRUNCATE is not a token in CAL"),
    ("DROP TABLE users", "CAL-E002", "DROP takes TEMPLATE or QUERY, never a table"),
];

#[test]
fn every_rejection_leads_with_its_error_code() {
    for (query, want, why) in REPROS {
        let err = parse(query)
            .err()
            .unwrap_or_else(|| panic!("{why}: this query was ACCEPTED but must be rejected: {query:?}"));

        assert_eq!(
            err.code(),
            *want,
            "{why}: wrong code for {query:?} — got {} ({err})",
            err.code()
        );

        // The leading-token rule. Anything else and a pasted error message
        // stops being enough to find the variant.
        let shown = err.to_string();
        assert!(
            shown.starts_with(want),
            "{why}: Display must begin with {want}, got {shown:?}"
        );
    }
}

/// Generated inputs that trip the size caps. Kept out of the table above
/// because they are built, not typed.
#[test]
fn the_size_caps_report_their_own_codes() {
    let long = format!("RECALL facts WHERE subject = \"{}\"", "x".repeat(70_000));
    assert_eq!(parse(&long).unwrap_err().code(), "CAL-E001", "a query past the length cap");

    let members: Vec<String> = (0..2000).map(|i| format!("\"a{i}\"")).collect();
    let in_set = format!("RECALL facts WHERE subject IN ({})", members.join(","));
    assert_eq!(parse(&in_set).unwrap_err().code(), "CAL-E011", "an IN set past the cap");

    let piped = format!("RECALL facts{}", " | COUNT".repeat(50));
    assert_eq!(parse(&piped).unwrap_err().code(), "CAL-E012", "too many pipeline stages");

    let unioned = vec!["RECALL facts"; 50].join(" UNION ");
    assert_eq!(parse(&unioned).unwrap_err().code(), "CAL-E013", "too many set operands");

    let nested = format!(
        "RECALL facts WHERE {}subject = \"a\"{}",
        "(".repeat(40),
        ")".repeat(40)
    );
    assert_eq!(parse(&nested).unwrap_err().code(), "CAL-E007", "nesting past the depth cap");

    // Each cap must reject rather than silently truncate: a query that gets
    // quietly shortened returns a *wrong answer*, which is worse than an error.
    assert!(parse("RECALL facts LIMIT 10").is_ok(), "a query inside the caps must still parse");
}

/// Every code the engine can emit must fall inside a documented range.
///
/// The CAL section of `ERROR_CODES.md` deliberately registers **ranges** —
/// `CAL-E001`–`E019` for lexing/parsing, and so on — with the inline docs on
/// `CalError` as the per-variant source of truth. So the contract to enforce is
/// not "every code has its own row" but "no code escapes the table": a new
/// `CAL-E130` added without extending the ranges is undocumented, and codes are
/// append-only precisely so callers can match on them.
#[test]
fn every_emitted_cal_code_falls_inside_a_documented_range() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let registry = std::fs::read_to_string(root.join("ERROR_CODES.md"))
        .expect("ERROR_CODES.md is the registry and must be readable");
    let source = std::fs::read_to_string(root.join("crates/areev-cal/src/errors.rs"))
        .expect("errors.rs owns the code() mapping");

    // Ranges: `CAL-E001`–`E019`, plus singletons like `CAL-E060`.
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for line in registry.lines().filter(|l| l.contains("CAL-E")) {
        let nums: Vec<u32> = line
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| s.len() == 3)
            .filter_map(|s| s.parse().ok())
            .collect();
        match nums.len() {
            1 => ranges.push((nums[0], nums[0])),
            2 => ranges.push((nums[0], nums[1])),
            _ => {}
        }
    }
    assert!(!ranges.is_empty(), "no CAL ranges parsed out of ERROR_CODES.md");

    // Every code literal the mapping can return.
    let mut emitted: Vec<u32> = source
        .match_indices("\"CAL-E")
        .filter_map(|(i, _)| source.get(i + 6..i + 9))
        .filter_map(|s| s.parse().ok())
        .collect();
    emitted.sort_unstable();
    emitted.dedup();
    assert!(emitted.len() > 50, "expected the full CAL code set, found {}", emitted.len());

    let undocumented: Vec<u32> = emitted
        .iter()
        .copied()
        .filter(|n| !ranges.iter().any(|(lo, hi)| n >= lo && n <= hi))
        .collect();
    assert!(
        undocumented.is_empty(),
        "these CAL codes are emitted but fall outside every range in ERROR_CODES.md: {}",
        undocumented
            .iter()
            .map(|n| format!("CAL-E{n:03}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}
