//! Outbound allowlisting and credential brokering for connectors.
//!
//! ## Why this and not a sandbox
//!
//! A polling connector legitimately needs the network *and* the credential, so
//! isolation technology does not constrain the thing that actually goes wrong.
//! Put a connector in a container, a microVM, or a Wasm module with network
//! access, and it can still read the token it was handed and POST it anywhere.
//! That is not hypothetical: the January 2026 n8n community-node compromise
//! exfiltrated decrypted OAuth tokens, and the malicious node never violated any
//! sandbox — it read a credential it was given and made a request it was allowed
//! to make. Zapier runs each task in a Firecracker microVM and the same class of
//! attack works there.
//!
//! Two controls do constrain it, and neither needs an isolation runtime:
//!
//! 1. **A deny-by-default outbound allowlist**, so a connector can only reach
//!    the hosts its declaration names. Semantics borrowed from Fermyon Spin's
//!    `allowed_outbound_hosts`.
//! 2. **Credential brokering**, so the connector never holds the token at all.
//!    It calls a loopback broker unauthenticated and the broker attaches
//!    credentials on the way out. Cloudflare shipped this in April 2026 ("no
//!    token is ever passed into the sandbox"), Deno in February, and it is the
//!    whole of Nango's product.
//!
//! ## What this is not
//!
//! It raises the bar; it is not a boundary. Exfiltration *through* an allowed
//! host (encoding data into a draft, a label, a filename) still works, and
//! allowlisting by hostname cannot see through DNS tricks or domain fronting —
//! Claude Code is candid that its own proxy decides from the client-supplied
//! hostname without inspecting TLS. Saying so is part of shipping it.

use crate::error::{Result, TriggerError};

/// One entry of a connector's outbound allowlist.
///
/// Spelled as a URL prefix — `https://api.github.com`, `https://*.example.com`,
/// `http://localhost:8080`. Scheme and host must be present; a port defaults
/// per scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    scheme: String,
    /// Host, possibly with a leading `*.` wildcard.
    host: String,
    port: u16,
}

impl AllowedHost {
    pub fn parse(spec: &str) -> Result<AllowedHost> {
        let bad = |why: &str| TriggerError::Malformed {
            what: format!("allowed_outbound_hosts entry {spec:?}: {why}"),
        };
        let (scheme, rest) = spec.split_once("://").ok_or_else(|| {
            bad("must be a URL with a scheme, e.g. https://api.example.com")
        })?;
        if scheme != "http" && scheme != "https" {
            return Err(bad("scheme must be http or https"));
        }
        // A path would imply path-level authorisation, which this does not do.
        // Refusing is better than accepting and silently ignoring it.
        let rest = rest.trim_end_matches('/');
        if rest.contains('/') {
            return Err(bad("must not include a path — this allows hosts, not paths"));
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| bad("port is not a number"))?,
            ),
            None => (rest.to_string(), if scheme == "https" { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(bad("host is empty"));
        }
        // A bare `*` would allow the entire internet under the appearance of a
        // policy, which is worse than no policy at all because it reads as one.
        if host == "*" {
            return Err(bad(
                "a bare '*' allows the whole internet — name the hosts, or omit \
                 the allowlist entirely to say so explicitly",
            ));
        }
        Ok(AllowedHost { scheme: scheme.to_string(), host: host.to_lowercase(), port })
    }

    fn matches(&self, scheme: &str, host: &str, port: u16) -> bool {
        if self.scheme != scheme || self.port != port {
            return false;
        }
        let host = host.to_lowercase();
        match self.host.strip_prefix("*.") {
            // `*.example.com` covers `a.example.com` but NOT `example.com`
            // itself, and not `evil-example.com` — the dot is part of the match
            // precisely so a suffix check cannot be fooled by a longer name.
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => host == self.host,
        }
    }
}

/// A connector's outbound policy.
///
/// Note [`EgressPolicy::default`] is **deny-all**, which is deliberately NOT
/// the same as [`EgressPolicy::from_config`] with no config — that is
/// unrestricted. A policy constructed without anyone saying anything should
/// fail closed; a declaration that deliberately omits an allowlist is a
/// statement, and is treated as one.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    allowed: Vec<AllowedHost>,
    /// No allowlist declared at all. Permits everything, and is reported as
    /// such rather than being silently equivalent to a policy.
    unrestricted: bool,
}

// Written out rather than derived on purpose: the point is that deny-all is a
// deliberate choice, not whatever the field defaults happen to produce. If a
// field is added later, this forces someone to decide what it defaults to
// instead of inheriting an answer.
#[allow(clippy::derivable_impls)]
impl Default for EgressPolicy {
    fn default() -> Self {
        EgressPolicy { allowed: Vec::new(), unrestricted: false }
    }
}

impl EgressPolicy {
    /// Permit every destination. Spelled out so it can only be reached on
    /// purpose, never by writing `default()`.
    pub fn unrestricted() -> Self {
        EgressPolicy { allowed: Vec::new(), unrestricted: true }
    }

    /// Read the policy off a trigger's `config`.
    pub fn from_config(config: Option<&serde_json::Value>) -> Result<EgressPolicy> {
        let key = areev_core::types::config_keys::ALLOWED_OUTBOUND_HOSTS;
        let Some(list) = config.and_then(|c| c.get(key)) else {
            return Ok(EgressPolicy::unrestricted());
        };
        let entries = list.as_array().ok_or_else(|| TriggerError::Malformed {
            what: format!("{key} must be an array of URL prefixes"),
        })?;
        let mut allowed = Vec::new();
        for e in entries {
            let s = e.as_str().ok_or_else(|| TriggerError::Malformed {
                what: format!("{key} entries must be strings"),
            })?;
            allowed.push(AllowedHost::parse(s)?);
        }
        // An empty list is a real policy: allow nothing. Distinct from an
        // absent one, which allows everything.
        Ok(EgressPolicy { allowed, unrestricted: false })
    }

    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// May this connector reach `url`?
    pub fn permits(&self, url: &str) -> Result<()> {
        if self.unrestricted {
            return Ok(());
        }
        let (scheme, host, port) = split_url(url)?;
        if self.allowed.iter().any(|a| a.matches(&scheme, &host, port)) {
            Ok(())
        } else {
            Err(TriggerError::EgressRefused { trigger: String::new(), host: format!("{scheme}://{host}:{port}") })
        }
    }
}

/// Scheme, host, port from an absolute URL.
fn split_url(url: &str) -> Result<(String, String, u16)> {
    let bad = |why: &str| TriggerError::Malformed { what: format!("url {url:?}: {why}") };
    let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("not an absolute URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip userinfo: `https://evil.com@allowed.com/` is the classic way to
    // make a URL read as one host and resolve to another.
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if authority.is_empty() {
        return Err(bad("no host"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad("port is not a number"))?),
        None => (authority, if scheme == "https" { 443 } else { 80 }),
    };
    Ok((scheme.to_lowercase(), host.to_lowercase(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[&str]) -> EgressPolicy {
        EgressPolicy::from_config(Some(&serde_json::json!({
            "int:allowed_outbound_hosts": entries
        })))
        .unwrap()
    }

    #[test]
    fn an_exact_host_is_permitted_and_others_are_not() {
        let p = policy(&["https://api.github.com"]);
        assert!(p.permits("https://api.github.com/repos/x").is_ok());
        assert!(p.permits("https://evil.com/").is_err());
    }

    #[test]
    fn the_scheme_and_port_are_part_of_the_match() {
        let p = policy(&["https://api.example.com"]);
        assert!(p.permits("http://api.example.com/").is_err(), "downgrade must not pass");
        assert!(p.permits("https://api.example.com:8443/").is_err(), "another port is another host");
    }

    #[test]
    fn a_wildcard_covers_subdomains_but_not_the_apex_or_a_lookalike() {
        let p = policy(&["https://*.example.com"]);
        assert!(p.permits("https://api.example.com/").is_ok());
        assert!(p.permits("https://a.b.example.com/").is_ok());
        assert!(p.permits("https://example.com/").is_err(), "the apex is not a subdomain");
        // The dot in the match is what stops this: a bare suffix check would
        // accept it.
        assert!(p.permits("https://evil-example.com/").is_err(), "lookalike must not pass");
        assert!(p.permits("https://exampleXcom/").is_err());
    }

    #[test]
    fn userinfo_cannot_disguise_the_real_host() {
        // `https://api.github.com@evil.com/` resolves to evil.com while reading
        // as github. Parsing the authority after the last '@' is what closes it.
        let p = policy(&["https://api.github.com"]);
        assert!(p.permits("https://api.github.com@evil.com/").is_err());
    }

    #[test]
    fn case_is_not_significant_in_a_hostname() {
        let p = policy(&["https://API.GitHub.com"]);
        assert!(p.permits("https://api.github.com/").is_ok());
    }

    #[test]
    fn the_derived_default_would_have_been_permissive_so_it_is_written_out() {
        // A policy nobody configured must fail closed. `from_config(None)` —
        // a declaration that deliberately omits an allowlist — is the
        // permissive case, and is reached only by that path.
        assert!(!EgressPolicy::default().is_unrestricted());
        assert!(EgressPolicy::default().permits("https://anywhere.example/").is_err());
        assert!(EgressPolicy::unrestricted().is_unrestricted());
    }

    #[test]
    fn an_absent_allowlist_is_unrestricted_and_says_so() {
        let p = EgressPolicy::from_config(None).unwrap();
        assert!(p.is_unrestricted());
        assert!(p.permits("https://anywhere.example/").is_ok());
    }

    #[test]
    fn an_empty_allowlist_denies_everything() {
        // Distinct from an absent one. "Allow nothing" is a policy someone may
        // genuinely want, and it must not silently mean "allow everything".
        let p = policy(&[]);
        assert!(!p.is_unrestricted());
        assert!(p.permits("https://api.github.com/").is_err());
    }

    #[test]
    fn a_bare_star_is_refused_rather_than_accepted_as_a_policy() {
        // Accepting it would let a declaration look policed while permitting
        // the entire internet — worse than no policy, because it reads as one.
        let e = EgressPolicy::from_config(Some(&serde_json::json!({
            "int:allowed_outbound_hosts": ["https://*"]
        })))
        .unwrap_err();
        assert_eq!(e.code(), "TRG-E001");
        assert!(e.to_string().contains("whole internet"));
    }

    #[test]
    fn a_path_is_refused_because_this_allows_hosts_not_paths() {
        let e = AllowedHost::parse("https://api.github.com/repos").unwrap_err();
        assert!(e.to_string().contains("path"));
    }

    #[test]
    fn a_missing_scheme_is_refused() {
        assert!(AllowedHost::parse("api.github.com").is_err());
        assert!(AllowedHost::parse("ftp://files.example.com").is_err());
    }

    #[test]
    fn refusal_carries_the_code_and_names_the_destination() {
        let p = policy(&["https://api.github.com"]);
        let e = p.permits("https://evil.com/x").unwrap_err();
        assert_eq!(e.code(), "TRG-E009");
        assert!(e.to_string().contains("evil.com"), "{e}");
    }
}
