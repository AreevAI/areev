//! `areev auth mint|list|revoke` through the real binary (A1).
//!
//! The credential map is the one place a secret's *handling* is the feature —
//! minted not chosen, printed once, stored as a digest, revocable in
//! isolation — so these assertions are about what does and does not leave the
//! process, not just about exit codes.

use std::process::Command;
use tempfile::TempDir;

fn areev(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(args)
        .output()
        .expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn mint_prints_the_token_once_and_stores_only_its_digest() {
    let dir = TempDir::new().unwrap();
    let map = dir.path().join("areev-auth.json");
    let map = map.to_str().unwrap();

    let (ok, stdout, stderr) = areev(&[
        "auth", "mint", "--auth", map, "--id", "ci-runner", "--principal", "agent:ci", "--label",
        "CI runner",
    ]);
    assert!(ok, "mint failed: {stderr}");

    // STDOUT is the token and nothing else, so `... | pbcopy` or a pipe into
    // a secret store gets exactly the secret.
    let token = stdout.trim();
    assert!(token.starts_with("areev_pat_"), "prefix missing: {token:?}");
    assert!(token.len() > 40, "token too short to be 256 bits: {token:?}");
    assert_eq!(stdout.lines().count(), 1, "stdout must carry only the token");

    // The commentary goes to stderr, and says the token will not be shown again.
    assert!(stderr.contains("ONLY time"), "{stderr}");

    // The file stores the DIGEST, never the token — a stolen or synced copy
    // must be inert.
    let body = std::fs::read_to_string(map).unwrap();
    assert!(!body.contains(token), "the raw token must never be written to the map");
    assert!(body.contains("\"sha256\""), "{body}");
    assert!(body.contains("ci-runner") && body.contains("agent:ci"), "{body}");

    // Owner-only permissions: the map enumerates every principal and
    // credential id a deployment has.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(map).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "map must not be readable by others (got {mode:o})");
    }
}

#[test]
fn two_credentials_for_one_principal_revoke_independently() {
    let dir = TempDir::new().unwrap();
    let map = dir.path().join("a.json");
    let map = map.to_str().unwrap();

    for id in ["laptop", "ci-runner"] {
        let (ok, _, err) =
            areev(&["auth", "mint", "--auth", map, "--id", id, "--principal", "user:pat"]);
        assert!(ok, "mint {id}: {err}");
    }

    let (ok, listing, _) = areev(&["auth", "list", "--auth", map]);
    assert!(ok);
    assert!(listing.contains("laptop") && listing.contains("ci-runner"), "{listing}");
    // Listing must be safe to paste into a ticket: ids and principals, never
    // a digest.
    assert!(!listing.contains("sha256"), "{listing}");

    // Revoking ONE leaves the principal's other credential working — the
    // whole point of per-credential ids.
    let (ok, _, err) = areev(&["auth", "revoke", "--auth", map, "--id", "ci-runner"]);
    assert!(ok, "{err}");
    assert!(err.contains("RESTART"), "revocation must say it needs a restart: {err}");

    let (_, listing, _) = areev(&["auth", "list", "--auth", map]);
    assert!(listing.contains("laptop"), "the other credential survives: {listing}");
    assert!(!listing.contains("ci-runner"), "{listing}");

    // Revoking something that is not there is an error, not a silent no-op:
    // "I revoked it" must never be reported for a credential still live under
    // a different id.
    let (ok, _, err) = areev(&["auth", "revoke", "--auth", map, "--id", "ci-runner"]);
    assert!(!ok, "revoking an absent id must fail");
    assert!(err.contains("no credential"), "{err}");
}

#[test]
fn a_duplicate_id_is_refused_before_a_token_is_minted() {
    let dir = TempDir::new().unwrap();
    let map = dir.path().join("a.json");
    let map = map.to_str().unwrap();

    let (ok, _, _) = areev(&["auth", "mint", "--auth", map, "--id", "dup", "--principal", "p"]);
    assert!(ok);
    let before = std::fs::read_to_string(map).unwrap();

    let (ok, stdout, err) =
        areev(&["auth", "mint", "--auth", map, "--id", "dup", "--principal", "q"]);
    assert!(!ok, "duplicate id must be refused");
    assert!(err.contains("already has a credential"), "{err}");
    // Nothing was printed and nothing was written: an operator must never end
    // up holding a live secret that was never recorded.
    assert!(stdout.trim().is_empty(), "no token may be printed on failure: {stdout:?}");
    assert_eq!(std::fs::read_to_string(map).unwrap(), before, "the map must be untouched");
}

#[test]
fn expiry_is_recorded_and_reported() {
    let dir = TempDir::new().unwrap();
    let map = dir.path().join("a.json");
    let map = map.to_str().unwrap();

    let (ok, _, _) = areev(&[
        "auth", "mint", "--auth", map, "--id", "short", "--principal", "p", "--expires", "1d",
    ]);
    assert!(ok);
    let (_, listing, _) = areev(&["auth", "list", "--auth", map]);
    assert!(listing.contains("expires 20"), "expiry should be listed: {listing}");

    // A lifetime nobody can interpret is refused up front, rather than read
    // as "never expires".
    let (ok, _, err) = areev(&[
        "auth", "mint", "--auth", map, "--id", "bad", "--principal", "p", "--expires", "90x",
    ]);
    assert!(!ok, "an unparseable --expires must refuse");
    assert!(err.contains("--expires"), "{err}");
}

#[test]
fn auth_needs_no_db_and_refuses_unknown_subcommands() {
    let dir = TempDir::new().unwrap();
    let map = dir.path().join("a.json");
    let map = map.to_str().unwrap();

    // No --db anywhere, and no "using default memory" line: the credential
    // map names no memory, so implying an association would be a lie.
    let (ok, _, err) =
        areev(&["auth", "mint", "--auth", map, "--id", "x", "--principal", "p"]);
    assert!(ok, "{err}");
    assert!(!err.contains("default memory"), "auth must not resolve a database: {err}");

    // An unknown subcommand prints the usage rather than doing something.
    let (ok, _, err) = areev(&["auth", "rotate", "--auth", map]);
    assert!(!ok);
    assert!(err.contains("mint, list, revoke"), "{err}");

    // And --auth is required: there is no default credential map path.
    let (ok, _, err) = areev(&["auth", "list"]);
    assert!(!ok);
    assert!(err.contains("--auth"), "{err}");
}
