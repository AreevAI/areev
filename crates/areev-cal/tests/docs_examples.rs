//! Every CAL example in `docs/cal-reference.md` must parse.
//!
//! Three separate bug reports (#48, #51, #54) were all the same failure: the
//! reference documented a shape the grammar does not have, and nothing caught
//! it because the examples were prose. They are executable now.
//!
//! Scope is deliberately *parse*, not *execute*: an example's results depend on
//! fixture data, but its syntax is a promise the parser can check on its own.
//! A statement that parses but silently ignores a clause is a different class
//! of bug — see `recall_tuning_cal_tests.rs`.

use areev_cal::parser::parse;

const REFERENCE: &str = include_str!("../../../docs/cal-reference.md");

/// Placeholder hashes in the docs are elided (`sha256:a1b2c3d4...`,
/// `sha256:<hash>`) because a real 64-hex address is unreadable in prose.
/// Substitute one so the example's *shape* is still checked.
const REAL_HASH: &str =
    "sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d";

/// Extract the ```sql fences — the doc's convention is that these are runnable
/// examples, while grammar templates (with `<placeholder>` metasyntax) sit in
/// unlabeled fences.
fn sql_fences(md: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current: Option<(usize, Vec<&str>)> = None;
    for (i, line) in md.lines().enumerate() {
        match (&mut current, line.trim_end()) {
            (None, "```sql") => current = Some((i + 2, Vec::new())),
            (Some((start, body)), "```") => {
                out.push((*start, body.join("\n")));
                let _ = start;
                current = None;
            }
            (Some((_, body)), l) => body.push(l),
            _ => {}
        }
    }
    out
}

/// One fence holds several examples, separated by blank lines. Comment-only
/// chunks (the "these FAIL by design" block) are documentation about what does
/// *not* parse, so they are skipped rather than asserted on.
fn examples(fence: &str) -> Vec<String> {
    fence
        .split("\n\n")
        .map(|chunk| {
            chunk
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .map(|q| q.replace("sha256:<hash>", REAL_HASH).replace("sha256:a1b2c3d4...", REAL_HASH))
        .filter(|q| !q.trim().is_empty())
        .flat_map(|chunk| split_statements(&chunk))
        .collect()
}

/// A chunk is usually one statement, but several fences list independent
/// one-liners on consecutive lines (the four `RECALL` forms in §3.1, the three
/// pipeline examples in §4). Those are the lines a reader copies, so check them
/// individually — but only when each line really does stand alone, otherwise a
/// genuinely multi-line statement would be shredded into fragments.
fn split_statements(chunk: &str) -> Vec<String> {
    let lines: Vec<&str> = chunk.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() > 1 && lines.iter().all(|l| parse(l).is_ok()) {
        return lines.into_iter().map(str::to_string).collect();
    }
    vec![chunk.to_string()]
}

#[test]
fn every_sql_example_in_the_cal_reference_parses() {
    let fences = sql_fences(REFERENCE);
    assert!(
        fences.len() > 10,
        "extracted only {} sql fences — the extractor is broken, not the docs",
        fences.len()
    );

    let mut failures = Vec::new();
    let mut checked = 0usize;
    for (line, fence) in &fences {
        for q in examples(fence) {
            checked += 1;
            if let Err(e) = parse(&q) {
                failures.push(format!(
                    "docs/cal-reference.md:{line}\n    query: {}\n    error: {e}",
                    q.replace('\n', "\n           ")
                ));
            }
        }
    }

    assert!(
        checked > 30,
        "only {checked} examples extracted from {} fences",
        fences.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {checked} documented CAL examples do not parse:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// §4's stage table and the parser's own error message must list the same
/// stages. The table grew a `| WHERE` row the grammar never had (#54); the
/// parser's error is the authority, so pin the doc to it.
#[test]
fn the_pipeline_stage_table_matches_the_grammar() {
    let err = parse(r#"RECALL facts WHERE subject = "john" | WHERE relation = "prefers""#)
        .expect_err("`| WHERE` is not a pipeline stage");
    let msg = err.to_string();
    // Compare against the parenthesized list only. The full message ends with
    // "…, found WHERE", so a substring search over all of it says every stage
    // is supported — including the one that just got rejected.
    let accepted = msg
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(list, _)| list.to_string())
        .unwrap_or_else(|| panic!("parser error no longer lists the stages: {msg}"));

    let table = REFERENCE
        .lines()
        .filter(|l| l.starts_with("| `\\| "))
        .flat_map(|l| {
            // "| `\| SELECT f1, f2` | Keep only these fields |" → "SELECT"
            l.trim_start_matches("| `\\| ")
                .split('`')
                .next()
                .map(|s| s.trim().to_string())
        })
        .collect::<Vec<_>>();
    assert!(!table.is_empty(), "no pipeline stage rows found in §4");

    for row in &table {
        // Rows spell arguments and alternatives ("ORDER BY field [ASC|DESC]",
        // "LIMIT n" / "OFFSET n"); the stage keyword is the leading word(s).
        let keyword = row.split([' ', '`']).next().unwrap_or(row);
        assert!(
            accepted.contains(keyword),
            "§4 documents a `| {row}` stage, but the parser does not accept it \
             (it accepts: {accepted})"
        );
    }
}
