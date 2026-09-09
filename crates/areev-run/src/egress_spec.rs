//! The host's egress configuration as the spec strings every surface takes.
//!
//! `--credential`, `--allow-host`, `--tool-egress`, `--credential-ttl` and
//! `--resolver-env` were parsed in the CLI, which made the credential broker
//! CLI-only (#201): a host driving runs through a binding could pin a
//! `wasm32-areev-io` tool and still have every `areev::fetch` fail, because
//! nothing answered it. This is the ONE parser — the CLI flags, the same-named
//! binding parameters and the `$AREEV_RUN_*` variables all build an
//! [`EgressSpec`] and call [`EgressSpec::build`], so a spec that works on the
//! CLI works verbatim from Node, Python and `areev serve`.

use std::collections::BTreeMap;

use crate::broker::{Broker, CallerGrant, CredentialSource, EgressGrants};
use crate::egress::EgressPolicy;

/// The five settings, exactly as the CLI flags spell them.
///
/// Every field is optional; [`EgressSpec::build`] returns `None` when none of
/// the three that configure a broker (`credentials`, `allow_hosts`,
/// `tool_egress`) is set, which leaves tools exactly as they were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EgressSpec {
    /// `--credential`: `name=ENV_VAR[@principal]`, `name[@principal]=cmd:COMMAND`
    /// or `name[@principal]=vault:PATH#FIELD`, comma-separated.
    pub credentials: Option<String>,
    /// `--allow-host`: URL prefixes, comma-separated. Absent means unrestricted.
    pub allow_hosts: Option<String>,
    /// `--tool-egress`: `tool:cred[@host]+cred[@host]:METHOD+METHOD`, comma-separated.
    pub tool_egress: Option<String>,
    /// `--credential-ttl`: how long a minted credential may be reused.
    pub credential_ttl_secs: Option<u64>,
    /// `--resolver-env`: the variables a resolver command may see, comma-separated.
    pub resolver_env: Option<String>,
}

/// The out-of-band spellings, server-bound like `$AREEV_RUN_TOOL_CMD`.
pub const ENV_CREDENTIAL: &str = "AREEV_RUN_CREDENTIAL";
pub const ENV_ALLOW_HOST: &str = "AREEV_RUN_ALLOW_HOST";
pub const ENV_TOOL_EGRESS: &str = "AREEV_RUN_TOOL_EGRESS";
pub const ENV_CREDENTIAL_TTL: &str = "AREEV_RUN_CREDENTIAL_TTL";
pub const ENV_RESOLVER_ENV: &str = "AREEV_RUN_RESOLVER_ENV";

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl EgressSpec {
    /// Whether anything here configures a broker at all.
    pub fn configures_broker(&self) -> bool {
        self.credentials.is_some() || self.allow_hosts.is_some() || self.tool_egress.is_some()
    }

    /// The `$AREEV_RUN_*` spellings alone — what `areev serve` reads, since an
    /// MCP client must never be able to hand the server a credential grant.
    /// An empty variable counts as unset.
    pub fn from_env() -> Result<Self, String> {
        Self::default().with_env_fallback()
    }

    /// Fill every unset field from its `$AREEV_RUN_*` variable. A set field
    /// wins: the argument in front of you never loses to a variable inherited
    /// from a shell you cannot see.
    pub fn with_env_fallback(mut self) -> Result<Self, String> {
        self.credentials = self.credentials.or_else(|| env_nonempty(ENV_CREDENTIAL));
        self.allow_hosts = self.allow_hosts.or_else(|| env_nonempty(ENV_ALLOW_HOST));
        self.tool_egress = self.tool_egress.or_else(|| env_nonempty(ENV_TOOL_EGRESS));
        self.resolver_env = self.resolver_env.or_else(|| env_nonempty(ENV_RESOLVER_ENV));
        if self.credential_ttl_secs.is_none() {
            if let Some(v) = env_nonempty(ENV_CREDENTIAL_TTL) {
                self.credential_ttl_secs = Some(v.parse::<u64>().map_err(|_| {
                    format!("${ENV_CREDENTIAL_TTL}: expected whole seconds, got {v:?}")
                })?);
            }
        }
        Ok(self)
    }

    fn resolver_env_list(&self) -> Vec<String> {
        self.resolver_env
            .iter()
            .flat_map(|v| v.split(','))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Parse `credentials` into sources, with each one's owning principal
    /// (`name=VAR@principal` / `name@principal=…`) beside it. Resolver
    /// settings ride with each dynamic source — one parse, one meaning.
    ///
    /// An env-var value is READ here, from a variable the host named; a
    /// `cmd:`/`vault:` source reads nothing yet and is minted per TTL window
    /// inside the broker. An unset variable is refused, never dropped: a
    /// dropped credential surfaces hours later as someone else's `401`.
    pub fn credential_sources(
        &self,
    ) -> Result<Vec<(String, CredentialSource, Option<String>)>, String> {
        let ttl_secs = self.credential_ttl_secs;
        let resolver_env = self.resolver_env_list();
        let mut out = Vec::new();
        for pair in self.credentials.iter().flat_map(|v| v.split(',')) {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (lhs, spec) = pair.split_once('=').ok_or_else(|| {
                format!(
                    "--credential: expected name=ENV_VAR[@principal], name[@principal]=cmd:COMMAND, \
                     or name[@principal]=vault:PATH#FIELD, got {pair:?} — note that a comma \
                     separates credentials, so a resolver command containing one belongs in a script"
                )
            })?;
            // The principal may be named on the NAME side, which is the only
            // unambiguous place for a `cmd:` source: a command can contain '@'
            // anywhere, so splitting one on it would bind the credential to a
            // principal the operator never wrote (#113).
            let (name, name_owner) = match lhs.trim().split_once('@') {
                Some((n, p)) if !p.trim().is_empty() => (n.trim(), Some(p.trim().to_string())),
                Some(_) => {
                    return Err(format!(
                        "--credential {pair:?}: empty principal after '@' — write name=SOURCE for an \
                         unbound credential, or name@principal=SOURCE to bind one"
                    ))
                }
                None => (lhs.trim(), None),
            };
            if name.is_empty() {
                return Err(format!("--credential {pair:?}: the credential has no name"));
            }
            let (source, spec_owner) = CredentialSource::from_spec(spec.trim())?;
            let owner = match (name_owner, spec_owner) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "--credential {pair:?}: names a principal on both sides of '=' — pick one"
                    ))
                }
                (a, b) => a.or(b),
            };
            out.push((name.to_string(), source.with_resolver_config(ttl_secs, &resolver_env), owner));
        }
        Ok(out)
    }

    /// The credentials by name, owners dropped — what a trigger's own
    /// connector-poll broker takes: it is the trigger's standing egress, not
    /// a per-user run, so `@principal` governs only the runs a firing starts.
    pub fn unowned_credentials(&self) -> Result<BTreeMap<String, CredentialSource>, String> {
        Ok(self
            .credential_sources()?
            .into_iter()
            .map(|(name, source, _owner)| (name, source))
            .collect())
    }

    /// `allow_hosts` as a policy. Absent means unrestricted, and is reported
    /// as such rather than silently reading as a policy.
    pub fn policy(&self) -> Result<EgressPolicy, String> {
        match &self.allow_hosts {
            None => Ok(EgressPolicy::unrestricted()),
            Some(list) => {
                let entries: Vec<serde_json::Value> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(|h| serde_json::json!(h))
                    .collect();
                EgressPolicy::from_config(Some(&serde_json::json!({
                    "int:allowed_outbound_hosts": entries
                })))
            }
        }
    }

    /// `tool_egress` as per-tool grants.
    pub fn grants(&self) -> Result<EgressGrants, String> {
        let mut grants = EgressGrants::new();
        for spec in self.tool_egress.iter().flat_map(|v| v.split(',')) {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            // A URL anywhere in the spec is an operator writing `cred@https://host`
            // for the #112 pairing. Split on ':' that would leave `https` sitting
            // in the host position — a pairing that matches nothing while reading
            // as a restriction. Caught here, where the whole spec is still intact
            // and the message can say what to write instead.
            if spec.contains("://") {
                return Err(format!(
                    "--tool-egress {spec:?}: pair a credential with a BARE hostname \
                     (cred@api.example.com), not a URL — this spec is colon-delimited, so a scheme \
                     or port would tear it apart; scheme and port are narrowed by --allow-host"
                ));
            }
            let mut parts = spec.split(':');
            let tool = parts.next().unwrap_or("").trim();
            if tool.is_empty() {
                return Err(format!(
                    "--tool-egress: expected tool:cred[@host]+cred[@host]:METHOD+METHOD, got {spec:?}"
                ));
            }
            let mut g = CallerGrant::new();
            for c in parts.next().unwrap_or("").split('+').map(str::trim) {
                if c.is_empty() {
                    continue;
                }
                // `cred@host` pairs the credential with where it may be sent
                // (#112); a bare `cred` keeps the older any-host meaning.
                g = match c.split_once('@') {
                    Some((name, host)) => {
                        let name = name.trim();
                        if name.is_empty() {
                            return Err(format!(
                                "--tool-egress {spec:?}: {c:?} has no credential name before '@'"
                            ));
                        }
                        let host = crate::AllowedHost::parse_host_pattern(host, "--tool-egress")?;
                        g.credential_for(name, vec![host])
                    }
                    None => g.credential(c),
                };
            }
            for m in parts.next().unwrap_or("").split('+').map(str::trim) {
                if m.is_empty() {
                    continue;
                }
                // Validated rather than accepted verbatim: an unrecognized token
                // here used to become a method nothing would ever match, so a
                // typo — or a port that survived the colon split — produced a
                // grant that silently refused every write at runtime.
                let upper = m.to_ascii_uppercase();
                if !matches!(upper.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE") {
                    // A field of digits is almost always a port that survived the
                    // colon split (`cred@host:8443`), so say that rather than
                    // leaving the operator to work back from "8443 is not a method".
                    let hint = if m.chars().all(|c| c.is_ascii_digit()) {
                        " — if you meant a port, drop it: a credential↔host pairing \
                         names the host, and the port is narrowed by --allow-host"
                    } else {
                        ""
                    };
                    return Err(format!(
                        "--tool-egress {spec:?}: {m:?} is not an HTTP method; accepted: \
                         GET, HEAD, POST, PUT, PATCH, DELETE{hint}"
                    ));
                }
                g = g.method(&upper);
            }
            grants = grants.grant(tool, g);
        }
        Ok(grants)
    }

    /// Start the broker this spec describes, or `None` when it describes
    /// none. Credential owners are bound on the way out.
    pub fn build(&self) -> Result<Option<Broker>, String> {
        if !self.configures_broker() {
            return Ok(None);
        }
        let mut credentials = BTreeMap::new();
        let mut owners: Vec<(String, String)> = Vec::new();
        for (name, source, owner) in self.credential_sources()? {
            if let Some(o) = owner {
                owners.push((name.clone(), o));
            }
            credentials.insert(name, source);
        }
        let broker = Broker::start(self.policy()?, credentials, self.grants()?, "RUN-E022")?;
        for (name, owner) in owners {
            broker.bind_credential_owner(&name, &owner);
        }
        Ok(Some(broker))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(credentials: &str, hosts: &str, egress: &str) -> EgressSpec {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        EgressSpec {
            credentials: some(credentials),
            allow_hosts: some(hosts),
            tool_egress: some(egress),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_set_builds_no_broker() {
        assert!(EgressSpec::default().build().unwrap().is_none());
        assert!(!EgressSpec::default().configures_broker());
    }

    #[test]
    fn the_cli_grammar_parses_verbatim() {
        std::env::set_var("AREEV_TEST_SPEC_TOK", "s3cret");
        let s = spec(
            "zoho=AREEV_TEST_SPEC_TOK@user:a,sheets@user:b=cmd:echo t",
            "https://books.zoho.com,https://sheets.googleapis.com",
            "sync:zoho@books.zoho.com+sheets:POST+GET,parse::",
        );
        let creds = s.credential_sources().unwrap();
        assert_eq!(creds.len(), 2);
        assert_eq!(creds[0].0, "zoho");
        assert_eq!(creds[0].2.as_deref(), Some("user:a"));
        assert_eq!(creds[1].0, "sheets");
        assert_eq!(creds[1].2.as_deref(), Some("user:b"));
        assert!(s.unowned_credentials().unwrap().contains_key("sheets"));
        let broker = s.build().unwrap().expect("a broker");
        assert!(broker.token_for("sync").is_some());
        assert!(broker.token_for("parse").is_some(), "a grant naming nothing still mints a token");
        assert!(broker.token_for("stranger").is_none());
        std::env::remove_var("AREEV_TEST_SPEC_TOK");
    }

    #[test]
    fn the_cli_refusals_are_the_same_refusals() {
        let e = spec("zoho", "", "").build().err().expect("a refusal");
        assert!(e.contains("--credential: expected name=ENV_VAR"), "{e}");
        let e = spec("zoho=AREEV_UNSET_VAR_FOR_TEST", "", "").build().err().expect("a refusal");
        assert!(e.contains("AREEV_UNSET_VAR_FOR_TEST"), "{e}");
        let e = spec("", "", "sync:zoho@https://x.com:POST").build().err().expect("a refusal");
        assert!(e.contains("BARE hostname"), "{e}");
        let e = spec("", "", "sync:zoho:8443").build().err().expect("a refusal");
        assert!(e.contains("if you meant a port"), "{e}");
        let e = spec("", "", ":zoho:POST").build().err().expect("a refusal");
        assert!(e.contains("expected tool:cred"), "{e}");
    }

    #[test]
    fn the_env_fallback_fills_only_what_is_unset() {
        std::env::set_var("AREEV_RUN_TOOL_EGRESS", "x::");
        std::env::set_var("AREEV_RUN_CREDENTIAL_TTL", "  ");
        let s = EgressSpec { tool_egress: Some("y::".into()), ..Default::default() }
            .with_env_fallback()
            .unwrap();
        assert_eq!(s.tool_egress.as_deref(), Some("y::"), "the set value wins");
        assert_eq!(s.credential_ttl_secs, None, "an empty variable is unset");
        let s = EgressSpec::from_env().unwrap();
        assert_eq!(s.tool_egress.as_deref(), Some("x::"));
        std::env::set_var("AREEV_RUN_CREDENTIAL_TTL", "soon");
        let e = EgressSpec::from_env().unwrap_err();
        assert!(e.contains("AREEV_RUN_CREDENTIAL_TTL"), "{e}");
        std::env::remove_var("AREEV_RUN_TOOL_EGRESS");
        std::env::remove_var("AREEV_RUN_CREDENTIAL_TTL");
    }
}
