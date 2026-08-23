//! The vocabulary of the Tool grain's `capabilities` field, and the host-pattern
//! grammar it shares with the outbound allowlist (#101).
//!
//! ## Why this lives in core
//!
//! `capabilities` is a field on a grain, so its grammar belongs beside the
//! grain — but the reason is sharper than tidiness. **Three** layers have to
//! agree on what a declaration means, and they sit at different heights in the
//! dependency graph:
//!
//! | Layer | Crate | Asks |
//! |---|---|---|
//! | write validation | `areev-cal` | may this grain be stored? |
//! | start-time freeze | `areev-run` (manifest) | may this run start? |
//! | per-call enforcement | `areev-run` (broker) | may this call go out? |
//!
//! `areev-cal` sits *below* `areev-run`, so a parser living in the driver is
//! unreachable from the write path. The alternative — a looser structural check
//! in CAL and the real grammar in the driver — is exactly how a tool becomes
//! writable and then unrunnable, discovered on the run someone needed it for.
//! One parser, at the bottom, and every layer calls it.
//!
//! ## Declaration, never authorization
//!
//! This is the half that REPLICATES. It cannot grant anything: the effective
//! set is `declared ∩ host-granted`, so a declaration only ever narrows what an
//! operator already permitted with `--allow-host` / `--credential` /
//! `--tool-egress`. A grain that authorized its own egress would be a
//! permission arriving in the same bundle as the code it authorizes — the exact
//! thing `--allow-executor` exists to refuse for the code itself.

use std::collections::BTreeSet;

/// Config errors are plain messages; each layer wraps them in its own error.
type Result<T> = std::result::Result<T, String>;

/// One entry of an outbound allowlist or a declared capability.
///
/// Spelled as a URL prefix — `https://api.github.com`, `https://*.example.com`,
/// `http://localhost:8080`. Scheme and host must be present; a port defaults
/// per scheme. Grammar borrowed from Fermyon Spin's `allowed_outbound_hosts`.
///
/// Shared by [`HttpCapability`] and `areev_run::EgressPolicy` on purpose: a
/// declaration must not be readable more loosely than the grant it has to fit
/// inside, and two matchers is two readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    scheme: String,
    /// Host, possibly with a leading `*.` wildcard.
    host: String,
    port: u16,
}

impl AllowedHost {
    pub fn parse(spec: &str) -> Result<AllowedHost> {
        Self::parse_labeled(spec, "allowed_outbound_hosts")
    }

    /// Same grammar, but the error names the field the entry came from — a
    /// `capabilities.http.hosts` mistake should not report itself as an
    /// allowlist mistake.
    pub fn parse_labeled(spec: &str, field: &str) -> Result<AllowedHost> {
        let bad = |why: &str| format!("{field} entry {spec:?}: {why}");
        let (scheme, rest) = spec.split_once("://").ok_or_else(|| {
            bad("must be a URL with a scheme, e.g. https://api.example.com")
        })?;
        if scheme != "http" && scheme != "https" {
            return Err(bad("scheme must be http or https"));
        }
        // A path would imply path-level authorisation, which the ALLOWLIST does
        // not do. A capability declares `path_prefixes` separately, so the host
        // entry stays a host in both readings.
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

    pub fn matches(&self, scheme: &str, host: &str, port: u16) -> bool {
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

    /// Does this entry admit `url`? Fails closed on a URL that will not parse.
    pub fn permits_url(&self, url: &str) -> bool {
        match split_url(url) {
            Ok((scheme, host, port)) => self.matches(&scheme, &host, port),
            Err(_) => false,
        }
    }
}

/// Scheme, host, port from an absolute URL.
pub fn split_url(url: &str) -> Result<(String, String, u16)> {
    let bad = |why: &str| format!("url {url:?}: {why}");
    let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("not an absolute URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip userinfo: `https://evil.com@allowed.com/` is the classic way to
    // make a URL read as one host and resolve to another.
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if authority.is_empty() {
        return Err(bad("no host"));
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    // A bracketed IPv6 host (`[::1]`, `[::1]:8080`) is colon-dense, so the port
    // is only ever after the CLOSING bracket — splitting on the last colon
    // would tear the address apart. The brackets are kept as part of the host
    // so it round-trips and matches an allowlist entry spelled the same way.
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').ok_or_else(|| bad("unterminated IPv6 literal"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad("port is not a number"))?,
            None if after.is_empty() => default_port,
            None => return Err(bad("unexpected text after IPv6 literal")),
        };
        (format!("[{inside}]"), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| bad("port is not a number"))?),
            None => (authority.to_string(), default_port),
        }
    };
    Ok((scheme.to_lowercase(), host.to_lowercase(), port))
}

/// Shapes that make a literal prefix comparison lie about where a request
/// lands. See the call site in [`Declaration::permits`] for why these are
/// refused rather than normalized.
fn path_evades_prefix_match(path: &str) -> bool {
    if path.contains('\\') {
        return true;
    }
    // Percent-encoded dots and separators, any casing: the server may decode
    // `%2e` into the dot-segments checked below, and `%2f`/`%5c` into the very
    // separator the prefix comparison splits on.
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        return true;
    }
    // Dot-segments proper (RFC 3986 §5.2.4 removes them — upward for `..`).
    path.split('/').any(|seg| seg == ".." || seg == ".")
}

/// The path component of an absolute URL, query and fragment excluded.
/// Returns `/` when there is none, so a prefix of `/` matches everything.
pub fn url_path(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    match after_scheme.find('/') {
        Some(i) => after_scheme[i..].split(['?', '#']).next().unwrap_or("/").to_string(),
        None => "/".to_string(),
    }
}

/// Why a declared capability refused a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityDenied {
    /// The module declared no capability of this kind at all.
    Undeclared { kind: &'static str },
    /// The destination is outside what the module declared.
    Host { destination: String },
    /// The path is outside the declared prefixes.
    Path { path: String },
    /// The method is outside the declared set.
    Method { method: String },
    /// The credential is outside the declared set.
    Credential { name: String },
}

impl std::fmt::Display for CapabilityDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilityDenied::Undeclared { kind } => write!(
                f,
                "asked for a '{kind}' capability its Definition does not declare"
            ),
            CapabilityDenied::Host { destination } => write!(
                f,
                "tried to reach '{destination}', which its declared capability does not name"
            ),
            CapabilityDenied::Path { path } => write!(
                f,
                "tried to reach path '{path}', which is outside its declared path_prefixes"
            ),
            CapabilityDenied::Method { method } => write!(
                f,
                "tried to issue {method}, which its declared capability does not permit"
            ),
            CapabilityDenied::Credential { name } => write!(
                f,
                "asked for credential '{name}', which its declared capability does not name"
            ),
        }
    }
}

/// The `http` capability: where a module says it will go.
#[derive(Debug, Clone)]
pub struct HttpCapability {
    hosts: Vec<AllowedHost>,
    /// Empty = `GET`/`HEAD` only, the same deny-by-default reading a
    /// `CallerGrant` gives an unnamed method set.
    methods: BTreeSet<String>,
    /// Empty = any path on a declared host.
    path_prefixes: Vec<String>,
    /// Credential names this module may ask for. Empty = none.
    credentials: BTreeSet<String>,
}

/// Everything one Definition declares.
#[derive(Debug, Clone, Default)]
pub struct Declaration {
    http: Option<HttpCapability>,
}

impl Declaration {
    /// Parse the grain's `capabilities` field.
    ///
    /// Shape: an array of single-key objects, so the vocabulary can grow
    /// without the field changing shape. Unknown keys are **refused**, not
    /// ignored — a capability this build does not understand must not be
    /// silently dropped into "declares nothing", which would read as a
    /// narrower tool than the author wrote, on a build that cannot honour it.
    pub fn parse(value: &serde_json::Value) -> Result<Declaration> {
        let entries = value
            .as_array()
            .ok_or("'capabilities' must be an array of capability objects")?;
        let mut out = Declaration::default();
        for entry in entries {
            let obj = entry
                .as_object()
                .ok_or("each 'capabilities' entry must be an object")?;
            if obj.len() != 1 {
                return Err(
                    "each 'capabilities' entry must name exactly one capability, e.g. {\"http\": …}"
                        .into(),
                );
            }
            let (kind, body) = obj.iter().next().expect("len checked above");
            match kind.as_str() {
                "http" => {
                    if out.http.is_some() {
                        return Err("'capabilities' declares 'http' more than once".into());
                    }
                    out.http = Some(parse_http(body)?);
                }
                other => {
                    return Err(format!(
                        "'capabilities' entry '{other}' is not recognized; accepted: http"
                    ))
                }
            }
        }
        Ok(out)
    }

    /// Does this declaration admit the `areev::fetch` import at all?
    pub fn declares_http(&self) -> bool {
        self.http.is_some()
    }

    /// May the module make this call?
    ///
    /// The DECLARED half of `declared ∩ host-granted`; the broker checks the
    /// host grant separately and both must pass.
    pub fn permits(
        &self,
        url: &str,
        method: &str,
        credential: Option<&str>,
    ) -> std::result::Result<(), CapabilityDenied> {
        let Some(http) = &self.http else {
            return Err(CapabilityDenied::Undeclared { kind: "http" });
        };
        if !http.hosts.iter().any(|h| h.permits_url(url)) {
            return Err(CapabilityDenied::Host { destination: url.to_string() });
        }
        if !http.path_prefixes.is_empty() {
            let path = url_path(url);
            // A prefix match on the LITERAL path is bypassable by anything the
            // upstream normalizes after we compared: `/declared/../../admin`
            // resolves upward, `%2e%2e` decodes to `..` on enough servers to
            // matter, and some frameworks treat `\` as `/`. Normalizing here
            // would mean betting our normalization matches every upstream's,
            // so evasive shapes are refused outright instead — a legitimate
            // API path has no business containing any of them.
            if path_evades_prefix_match(&path)
                || !http.path_prefixes.iter().any(|p| path.starts_with(p.as_str()))
            {
                return Err(CapabilityDenied::Path { path });
            }
        }
        let permitted_method = if http.methods.is_empty() {
            matches!(method, "GET" | "HEAD")
        } else {
            http.methods.contains(method)
        };
        if !permitted_method {
            return Err(CapabilityDenied::Method { method: method.to_string() });
        }
        if let Some(name) = credential {
            if !http.credentials.contains(name) {
                return Err(CapabilityDenied::Credential { name: name.to_string() });
            }
        }
        Ok(())
    }
}

fn parse_http(body: &serde_json::Value) -> Result<HttpCapability> {
    let obj = body
        .as_object()
        .ok_or("'capabilities' entry 'http' must be an object")?;
    for k in obj.keys() {
        if !matches!(k.as_str(), "hosts" | "methods" | "path_prefixes" | "credentials") {
            return Err(format!(
                "'capabilities.http' key '{k}' is not recognized; \
                 accepted: hosts, methods, path_prefixes, credentials"
            ));
        }
    }
    // Deny by default, and say so at parse time: a capability with no hosts
    // declares nothing reachable, and accepting it silently would produce a
    // tool that looks capability-bearing and refuses every call at runtime.
    let raw = obj
        .get("hosts")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
        .ok_or("'capabilities.http.hosts' must be a non-empty array of URL prefixes")?;
    let mut hosts = Vec::new();
    for h in raw {
        let s = h
            .as_str()
            .ok_or("'capabilities.http.hosts' entries must be strings")?;
        hosts.push(AllowedHost::parse_labeled(s, "capabilities.http.hosts")?);
    }

    let methods: BTreeSet<String> = string_list(obj.get("methods"), "methods")?
        .into_iter()
        .map(|m| m.trim().to_ascii_uppercase())
        .collect();
    for m in &methods {
        if !matches!(m.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(format!(
                "'capabilities.http.methods' entry '{m}' is not an HTTP method this build issues; \
                 accepted: GET, HEAD, POST, PUT, PATCH, DELETE"
            ));
        }
    }

    let path_prefixes = string_list(obj.get("path_prefixes"), "path_prefixes")?;
    for p in &path_prefixes {
        if !p.starts_with('/') {
            return Err(format!(
                "'capabilities.http.path_prefixes' entry '{p}' must start with '/'"
            ));
        }
    }

    let credentials: BTreeSet<String> =
        string_list(obj.get("credentials"), "credentials")?.into_iter().collect();

    Ok(HttpCapability { hosts, methods, path_prefixes, credentials })
}

fn string_list(v: Option<&serde_json::Value>, field: &str) -> Result<Vec<String>> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| format!("'capabilities.http.{field}' must be an array of strings"))?;
    arr.iter()
        .map(|e| {
            e.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("'capabilities.http.{field}' entries must be strings"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decl(v: serde_json::Value) -> Declaration {
        Declaration::parse(&v).unwrap()
    }

    fn gmail() -> Declaration {
        decl(json!([{"http": {
            "hosts": ["https://gmail.googleapis.com"],
            "methods": ["POST"],
            "path_prefixes": ["/gmail/v1/users/me/"],
            "credentials": ["gmail"]
        }}]))
    }

    #[test]
    fn a_declared_call_is_permitted() {
        assert!(gmail()
            .permits(
                "https://gmail.googleapis.com/gmail/v1/users/me/messages/send",
                "POST",
                Some("gmail")
            )
            .is_ok());
    }

    #[test]
    fn a_module_with_no_declaration_may_do_nothing() {
        // Default deny: the absence of a capability is not permission.
        let d = Declaration::default();
        assert!(!d.declares_http());
        assert_eq!(
            d.permits("https://anywhere.example/", "GET", None),
            Err(CapabilityDenied::Undeclared { kind: "http" })
        );
    }

    #[test]
    fn the_host_the_path_the_method_and_the_credential_are_each_a_gate() {
        let d = gmail();
        assert!(matches!(
            d.permits("https://evil.example/gmail/v1/users/me/x", "POST", Some("gmail")),
            Err(CapabilityDenied::Host { .. })
        ));
        // The exfiltration case a host-only grant cannot express: an ALLOWED
        // host, a POST it is allowed to make, at an endpoint it never declared.
        assert!(matches!(
            d.permits("https://gmail.googleapis.com/upload/drive/v3/files", "POST", Some("gmail")),
            Err(CapabilityDenied::Path { .. })
        ));
        assert!(matches!(
            d.permits("https://gmail.googleapis.com/gmail/v1/users/me/x", "DELETE", Some("gmail")),
            Err(CapabilityDenied::Method { .. })
        ));
        assert!(matches!(
            d.permits("https://gmail.googleapis.com/gmail/v1/users/me/x", "POST", Some("sheets")),
            Err(CapabilityDenied::Credential { .. })
        ));
    }

    #[test]
    fn a_dot_segment_cannot_climb_out_of_a_declared_prefix() {
        // `starts_with` says yes; the upstream's RFC 3986 normalization says
        // `/admin`. The evasive shape is refused, not normalized — normalizing
        // here would bet our resolution matches every server's.
        let d = gmail();
        for evasive in [
            "https://gmail.googleapis.com/gmail/v1/users/me/../../../admin",
            "https://gmail.googleapis.com/gmail/v1/users/me/./x",
            "https://gmail.googleapis.com/gmail/v1/users/me/%2e%2e/upload",
            "https://gmail.googleapis.com/gmail/v1/users/me/%2E%2E/upload",
            "https://gmail.googleapis.com/gmail/v1/users/me/..%2fupload",
            "https://gmail.googleapis.com/gmail/v1/users/me/\\upload",
        ] {
            assert!(
                matches!(
                    d.permits(evasive, "POST", Some("gmail")),
                    Err(CapabilityDenied::Path { .. })
                ),
                "{evasive} must not pass the prefix"
            );
        }
        // And a benign dotted filename is NOT collateral damage: only whole
        // dot-SEGMENTS and encoded dots are evasive.
        assert!(d
            .permits("https://gmail.googleapis.com/gmail/v1/users/me/msg.2.json", "POST", Some("gmail"))
            .is_ok());
    }

    #[test]
    fn a_query_string_cannot_smuggle_a_path_past_the_prefix() {
        assert!(matches!(
            gmail().permits(
                "https://gmail.googleapis.com/upload?x=/gmail/v1/users/me/",
                "POST",
                Some("gmail")
            ),
            Err(CapabilityDenied::Path { .. })
        ));
    }

    #[test]
    fn declaring_no_methods_means_read_only() {
        // Same reading as a CallerGrant: naming nothing is not naming everything.
        let d = decl(json!([{"http": {"hosts": ["https://api.example.com"]}}]));
        assert!(d.permits("https://api.example.com/x", "GET", None).is_ok());
        assert!(matches!(
            d.permits("https://api.example.com/x", "POST", None),
            Err(CapabilityDenied::Method { .. })
        ));
    }

    #[test]
    fn declaring_no_credentials_means_none() {
        let d = decl(json!([{"http": {"hosts": ["https://api.example.com"]}}]));
        assert!(matches!(
            d.permits("https://api.example.com/x", "GET", Some("anything")),
            Err(CapabilityDenied::Credential { .. })
        ));
    }

    #[test]
    fn an_unknown_capability_kind_is_refused_rather_than_ignored() {
        // Ignoring it would read as a NARROWER tool than the author wrote, on
        // a build that cannot honour the declaration — fail closed and loudly.
        let e = Declaration::parse(&json!([{"filesystem": {"paths": ["/tmp"]}}])).unwrap_err();
        assert!(e.contains("filesystem"), "{e}");
    }

    #[test]
    fn a_capability_with_no_hosts_is_refused_at_parse() {
        for bad in [json!([{"http": {}}]), json!([{"http": {"hosts": []}}])] {
            let e = Declaration::parse(&bad).unwrap_err();
            assert!(e.contains("hosts"), "{e}");
        }
    }

    #[test]
    fn the_host_grammar_is_the_allowlist_grammar() {
        // A declaration must not be readable more loosely than an
        // `--allow-host` entry, so the same refusals apply — and the error
        // names the field it came from.
        let e = Declaration::parse(&json!([{"http": {"hosts": ["https://*"]}}])).unwrap_err();
        assert!(e.contains("whole internet"), "{e}");
        assert!(e.contains("capabilities.http.hosts"), "the error names its field: {e}");

        for bad in [
            json!([{"http": {"hosts": ["api.example.com"]}}]),
            json!([{"http": {"hosts": ["https://api.example.com/v1"]}}]),
            json!([{"http": {"hosts": ["ftp://files.example.com"]}}]),
        ] {
            assert!(Declaration::parse(&bad).is_err(), "{bad} must not parse");
        }

        // And the wildcard reads the same way it does in an allowlist.
        let d = decl(json!([{"http": {"hosts": ["https://*.example.com"]}}]));
        assert!(d.permits("https://api.example.com/x", "GET", None).is_ok());
        assert!(d.permits("https://example.com/x", "GET", None).is_err(), "the apex is not a subdomain");
        assert!(d.permits("https://evil-example.com/x", "GET", None).is_err(), "lookalike");
        assert!(d.permits("https://api.example.com@evil.com/x", "GET", None).is_err(), "userinfo");
    }

    #[test]
    fn a_malformed_shape_is_refused() {
        for bad in [
            json!({"http": {}}),
            json!([{"http": {}, "other": {}}]),
            json!(["http"]),
            json!([{"http": {"hosts": ["https://a.example"], "methods": ["TRACE"]}}]),
            json!([{"http": {"hosts": ["https://a.example"], "path_prefixes": ["gmail"]}}]),
            json!([{"http": {"hosts": ["https://a.example"], "nope": []}}]),
            json!([{"http": {"hosts": ["https://a.example"], "methods": "GET"}}]),
        ] {
            assert!(Declaration::parse(&bad).is_err(), "{bad} must not parse");
        }
    }

    #[test]
    fn a_bracketed_ipv6_authority_does_not_tear_at_its_colons() {
        // `rsplit_once(':')` would split inside the address; the port lives
        // only after the closing bracket.
        assert_eq!(
            split_url("http://[2606:4700::1]/x").unwrap(),
            ("http".into(), "[2606:4700::1]".into(), 80)
        );
        assert_eq!(
            split_url("https://[::1]:8443/y").unwrap(),
            ("https".into(), "[::1]".into(), 8443)
        );
        assert!(split_url("http://[::1").is_err(), "an unterminated literal is refused");
    }

    #[test]
    fn url_path_extraction() {
        assert_eq!(url_path("https://a.example/x/y?q=1#f"), "/x/y");
        assert_eq!(url_path("https://a.example"), "/");
        assert_eq!(url_path("https://a.example/"), "/");
    }

    #[test]
    fn an_unparseable_url_is_refused_rather_than_matched() {
        let h = AllowedHost::parse("https://api.example.com").unwrap();
        assert!(!h.permits_url("not-a-url"));
        assert!(!h.permits_url(""));
    }
}
