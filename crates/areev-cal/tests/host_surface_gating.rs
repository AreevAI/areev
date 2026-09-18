//! The gate on GHSA-rmrx-26f6-f97w: no ungated `with_store` on a surface a
//! bound principal can reach.
//!
//! `AreevFacade::with_store` applies no `AuthzSet` check. That is correct for
//! a host acting under its own authority, and wrong for the Python binding,
//! the Node binding and the MCP server — all three accept a principal
//! (`principal=`, `principal`, `areev serve --mcp --as`) and are documented to
//! fail closed for it (CAL 1.3 §9). Until 1.9.0 they reached the store through
//! `with_store` on nearly every typed method, so `principal=` restricted CAL
//! and nothing else: a read-only principal could read any namespace, write
//! through `remember()`, and erase through the memory tool's `delete`.
//!
//! The fix was to route those call sites through `store_read` / `store_write`
//! / `store_checked`. This test is what stops the next method from skipping
//! it: every surviving `with_store` in those three files must be named here
//! with the reason it needs no check. A new one fails the build until someone
//! decides which verb it takes.
//!
//! Matching is by the STORE METHOD called, not by line number, so the list
//! survives reformatting and only a genuine new call site trips it.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Store methods allowed to run ungated, and why. Anything else is a bug.
fn allowed() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        // ── no grain data crosses these ───────────────────────────────────
        ("open_warnings", "host diagnostics about this open; no grain data"),
        ("set_run_id", "ambient telemetry tag for the next call; reads nothing"),
        (
            "index_text_enabled",
            "capability probe (is there a text leg?) — reports no content",
        ),
        ("declared_embedding", "capability metadata: the file's embedding model and dim"),
        ("vector_index", "capability metadata: the ANN index name, if built"),
        // ── gated by an explicit `check_verb` immediately above ───────────
        ("forget_subject_with", "check_verb(Erase, ns) above"),
        ("subject_report_with", "check_verb(Read, ns) above"),
        ("subject_bundle_with", "check_verb(Read, ns) above"),
        ("forget_older_than", "check_verb(Erase, ns) above"),
        ("thread_tail", "check_verb(Read, ns) above"),
        ("trigger_state", "check_verb(Write, ns) above"),
        ("put_trigger_state", "check_verb(Write, ns) above"),
        // ── gated by `check_verb_all` over a namespace LIST ───────────────
        ("related", "check_verb_all(Read, scope) above — a walk spans a namespace set"),
        ("related_scoped", "check_verb_all(Read, scope) above — same walk, list form"),
        // ── memory-wide reads, filtered per row on the way out ────────────
        (
            "changes_since_scoped",
            "check_verb_all(Read, scope) above and filter_readable below",
        ),
        ("grains_derived_from", "filter_readable below — a hash edge grants no read"),
        ("anon_policies", "filter_readable below — one row per namespace"),
        ("anon_mappings", "filter_readable below — one row per namespace"),
    ])
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/areev-cal has a repo root")
        .to_path_buf()
}

/// The store method a `with_store` call reaches for: the first `m.foo(` or
/// `s.foo(` at or just after the call. Good enough to name the site, and
/// stable across rustfmt.
fn store_method(after: &str) -> Option<String> {
    let bytes: Vec<char> = after.chars().collect();
    let mut i = 0;
    while i + 2 < bytes.len() {
        let is_recv = (bytes[i] == 'm' || bytes[i] == 's') && bytes[i + 1] == '.';
        let boundary = i == 0 || !bytes[i - 1].is_alphanumeric() && bytes[i - 1] != '_';
        if is_recv && boundary {
            let mut j = i + 2;
            let mut name = String::new();
            while j < bytes.len() && (bytes[j].is_alphanumeric() || bytes[j] == '_') {
                name.push(bytes[j]);
                j += 1;
            }
            if j < bytes.len() && bytes[j] == '(' && !name.is_empty() {
                return Some(name);
            }
        }
        i += 1;
    }
    None
}

#[test]
fn principal_bindable_surfaces_have_no_ungated_store_access() {
    let root = repo_root();
    let files = [
        "crates/areev-py/src/lib.rs",
        "crates/areev-js/src/lib.rs",
        "crates/areev-mcp/src/lib.rs",
    ];
    let allowed = allowed();
    let mut offenders: Vec<String> = Vec::new();

    for rel in files {
        let path = root.join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for (idx, line) in src.lines().enumerate() {
            // Only real calls: skip prose in doc comments and this file's own
            // name for the method.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            let Some(col) = line.find(".with_store(") else { continue };
            // The closure body may start on the next line or two.
            let tail: String = src
                .lines()
                .skip(idx)
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
            let after = &tail[tail.find(".with_store(").unwrap_or(col)..];
            match store_method(after) {
                Some(m) if allowed.contains_key(m.as_str()) => {}
                Some(m) => offenders.push(format!("{rel}:{}  ungated store call: {m}(…)", idx + 1)),
                None => offenders
                    .push(format!("{rel}:{}  with_store whose store method could not be read", idx + 1)),
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "ungated `with_store` on a principal-bindable surface (GHSA-rmrx-26f6-f97w).\n\
         Each of these reaches the store with no authorization check, so a handle bound to a \
         restricted principal can do it anyway.\n\n{}\n\n\
         Fix: route it through `AreevFacade::store_read` / `store_write` / `store_checked` with \
         the verb and namespace it actually needs. If it genuinely needs no check — it reads no \
         grain data, or an explicit `check_verb`/`filter_readable` already covers it — add the \
         store method to `allowed()` in this file WITH the reason.",
        offenders.join("\n")
    );
}

/// The allowlist must not outlive its call sites: an entry nobody uses any
/// more is a hole held open for no reason.
#[test]
fn every_allowlist_entry_is_still_used() {
    let root = repo_root();
    let src: String = [
        "crates/areev-py/src/lib.rs",
        "crates/areev-js/src/lib.rs",
        "crates/areev-mcp/src/lib.rs",
    ]
    .iter()
    .map(|r| std::fs::read_to_string(root.join(r)).unwrap_or_default())
    .collect::<Vec<_>>()
    .join("\n");

    let stale: Vec<&str> = allowed()
        .keys()
        .copied()
        .filter(|m| !src.contains(&format!("{m}(")))
        .collect();
    assert!(
        stale.is_empty(),
        "allowlist entries with no remaining call site — delete them: {stale:?}"
    );
}
