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

use areev_core::types::capability::split_url;

/// Why a destination or method was refused. Deliberately code-free: this
/// module is shared by the trigger evaluator and the run driver, and each
/// reports the refusal under its own domain (`TRG-E009` / `RUN-E022`) the same
/// way a storage failure is `TRG-E010` in one and `RUN-E020` in the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressDenied {
    /// The destination is outside the declared allowlist.
    Host { destination: String },
    /// The HTTP method is not one this caller may issue.
    Method { method: String },
}

impl std::fmt::Display for EgressDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressDenied::Host { destination } => write!(
                f,
                "tried to reach '{destination}', which is outside its declared \
                 allowed_outbound_hosts"
            ),
            EgressDenied::Method { method } => write!(
                f,
                "tried to issue {method}, which it is not permitted — a caller that \
                 declares no methods may only read"
            ),
        }
    }
}

/// Config errors are plain messages; the host wraps them in its own error.
type Result<T> = std::result::Result<T, String>;

/// The allowlist entry grammar lives in `areev-core` (#101): the Tool grain's
/// `capabilities` field declares hosts with exactly this syntax, and
/// `areev-cal`'s write path sits below this crate and must read it the same
/// way. Two matchers would be two readings, and a declaration must never be
/// readable more loosely than the grant it has to fit inside.
pub use areev_core::types::capability::AllowedHost;

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
        let entries = list
            .as_array()
            .ok_or_else(|| format!("{key} must be an array of URL prefixes"))?;
        let mut allowed = Vec::new();
        for e in entries {
            let s = e
                .as_str()
                .ok_or_else(|| format!("{key} entries must be strings"))?;
            allowed.push(AllowedHost::parse(s)?);
        }
        // An empty list is a real policy: allow nothing. Distinct from an
        // absent one, which allows everything.
        Ok(EgressPolicy { allowed, unrestricted: false })
    }

    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// May this caller reach `url`?
    pub fn permits(&self, url: &str) -> std::result::Result<(), EgressDenied> {
        if self.unrestricted {
            return Ok(());
        }
        let (scheme, host, port) =
            split_url(url).map_err(|_| EgressDenied::Host { destination: url.to_string() })?;
        if self.allowed.iter().any(|a| a.matches(&scheme, &host, port)) {
            Ok(())
        } else {
            Err(EgressDenied::Host { destination: format!("{scheme}://{host}:{port}") })
        }
    }
}

/// Does `url` aim at loopback, link-local, private-range, or unspecified
/// address space — the destinations where "the internet" ends and *this
/// machine and its network* begin?
///
/// Grain-stored capability tools are refused these under an unrestricted
/// egress policy (#101): a memory that syncs in can declare any hosts it
/// likes, and a declaration alone must never be what authorizes a request to
/// the loopback console, a cloud metadata service, or a LAN
/// neighbour. An operator who genuinely wants a capability tool talking to a
/// local service names it in `--allow-host`, which is an explicit, auditable
/// act — exactly the shape the executor pin gives code.
///
/// Syntactic only, and honestly so: a public HOSTNAME that resolves to a
/// private address (DNS rebinding) is not caught here — that is the
/// documented limitation of hostname allowlisting in general. What this
/// closes is the literal form, which is what every off-the-shelf SSRF payload
/// uses first — including the alternate integer encodings (`2130706433`,
/// `0x7f000001`, `017700000001`, `127.1`) that a libc resolver accepts but
/// `Ipv4Addr::from_str` does not.
pub fn is_private_destination(url: &str) -> bool {
    let Ok((_, host, _)) = split_url(url) else {
        // Unparseable never reaches dispatch anyway; classify it as private so
        // this function fails closed if it is ever called first.
        return true;
    };
    // A single trailing dot is an explicitly fully-qualified name (`localhost.`,
    // `127.0.0.1.`) naming the very same destination; drop it before the
    // literal checks so it cannot read as a different, "public" host.
    let host = host.strip_suffix('.').unwrap_or(&host);
    // RFC 6761: `localhost` and anything under it is loopback by fiat.
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    // IP literals. IPv6 arrives bracketed from the authority.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    // Canonical dotted-quad first, then the historical `inet_aton` numeric
    // forms (decimal, hex, octal, and the short `127.1` variants). A C resolver
    // — and therefore ureq — collapses all of these to the same address, so
    // recognizing only the canonical spelling would leave `http://2852039166/`
    // (that is `169.254.169.254`) a straight path to the metadata service.
    if let Some(v4) = bare
        .parse::<std::net::Ipv4Addr>()
        .ok()
        .or_else(|| parse_ipv4_inet_aton(bare))
    {
        return v4_is_private(v4);
    }
    if let Ok(v6) = bare.parse::<std::net::Ipv6Addr>() {
        // An IPv4-mapped address is its IPv4 self wearing a coat.
        if let Some(v4) = v6.to_ipv4_mapped() {
            return v4_is_private(v4);
        }
        let seg = v6.segments();
        return v6.is_loopback()
            || v6.is_unspecified()
            // fc00::/7 unique-local, fe80::/10 link-local. Spelled out rather
            // than the std helpers so the check does not ride an MSRV.
            || (seg[0] & 0xfe00) == 0xfc00
            || (seg[0] & 0xffc0) == 0xfe80;
    }
    false
}

fn v4_is_private(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        // 100.64.0.0/10, RFC 6598 shared address space — carrier-grade NAT and
        // the node/pod ranges of many container platforms. `Ipv4Addr::is_private`
        // does NOT cover it, so without this a capability tool could reach an
        // internal neighbour on its declaration alone under an unrestricted
        // policy, the exact class of destination this gate exists to deny.
        || matches!(v4.octets(), [100, b, _, _] if (64..=127).contains(&b))
}

/// The historical `inet_aton` numeric host forms that `Ipv4Addr::from_str`
/// rejects but a libc resolver (`getaddrinfo`, hence ureq) still accepts: a
/// bare 32-bit integer (`2130706433`), hex (`0x7f000001`), octal
/// (`017700000001`), and the short 1–3 part forms whose final part spans the
/// remaining low bytes (`127.1`, `10.0x1`). Returns the address the form
/// collapses to, or `None` when the string is a genuine hostname or malformed.
///
/// This exists ONLY to classify private-space destinations; a value that does
/// not parse as one of these forms is left to the hostname path, not forced.
fn parse_ipv4_inet_aton(host: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in &parts {
        nums.push(parse_c_uint(p)?);
    }
    let addr: u32 = match nums.as_slice() {
        // One number is the whole 32-bit address.
        [a] => u32::try_from(*a).ok()?,
        // a.b — b spans the low 24 bits.
        [a, b] => {
            let a = u8::try_from(*a).ok()?;
            if *b > 0x00ff_ffff {
                return None;
            }
            (u32::from(a) << 24) | (*b as u32)
        }
        // a.b.c — c spans the low 16 bits.
        [a, b, c] => {
            let a = u8::try_from(*a).ok()?;
            let b = u8::try_from(*b).ok()?;
            if *c > 0x0000_ffff {
                return None;
            }
            (u32::from(a) << 24) | (u32::from(b) << 16) | (*c as u32)
        }
        // a.b.c.d — the ordinary four octets, each still writable in hex/octal.
        [a, b, c, d] => u32::from_be_bytes([
            u8::try_from(*a).ok()?,
            u8::try_from(*b).ok()?,
            u8::try_from(*c).ok()?,
            u8::try_from(*d).ok()?,
        ]),
        _ => return None,
    };
    Some(std::net::Ipv4Addr::from(addr))
}

/// One `inet_aton` part: `0x` hex, a leading-`0` octal, otherwise decimal.
/// Returned as `u64` so the single-part 32-bit form fits before the caller
/// bounds it. `None` on an empty part or a digit outside the chosen base.
fn parse_c_uint(part: &str) -> Option<u64> {
    let (radix, digits) = if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        (16, hex)
    } else if part.len() > 1 && part.starts_with('0') {
        (8, &part[1..])
    } else {
        (10, part)
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_alphanumeric()) {
        // Reject `+`/`-`/whitespace that `from_str_radix` would otherwise
        // tolerate — a numeric host form has none of them.
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// Do two absolute URLs share scheme, host and port?
///
/// This is the credential's binding across a redirect (#99). The broker
/// attaches a secret because the *caller's* destination satisfied the
/// allowlist; a `30x` moves the request somewhere the caller never named, so
/// the secret travels only when the origin is unchanged. Anything else — a
/// different host, a scheme downgrade, another port — is a new origin and gets
/// no credential, which is the same rule browsers and `curl --location` apply
/// and stricter than ureq's `SameHost` (that one ignores the port).
///
/// Fails closed: a URL that will not parse cannot be claimed to match.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (split_url(a), split_url(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Resolve a `Location` header against the URL that produced it.
///
/// `Location` is allowed to be relative (RFC 9110 §10.2.2), so following one by
/// hand means doing the resolution the HTTP client used to do for us. Four
/// shapes, and anything else is refused rather than guessed at — an
/// unresolvable `Location` must not become a request to somewhere unintended:
///
/// | Location | Resolves to |
/// |---|---|
/// | `https://b.example/x` | itself |
/// | `//b.example/x` | the base's scheme + it |
/// | `/x` | the base's origin + `/x` |
/// | `x` | the base's directory + `x` |
pub fn resolve_location(base: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    // A control character in a Location is a response-splitting attempt, not a
    // URL. Refuse before it can be concatenated into a request line.
    if location.chars().any(|c| c.is_control()) {
        return None;
    }
    // A reference that carries a scheme is absolute (RFC 3986 §4.2), and that
    // is decided by a bare `scheme:` — NOT by the presence of `://`. Checking
    // for `://` alone let `javascript:alert(1)` and `mailto:x@y` fall through
    // to relative resolution and come back as innocuous-looking paths under
    // the base's host, which is the wrong answer even though it is a safe one.
    if let Some(scheme) = leading_scheme(location) {
        if scheme != "http" && scheme != "https" {
            return None;
        }
        // It claims http(s); it still has to parse as one.
        split_url(location).ok()?;
        return Some(location.to_string());
    }
    let (base_scheme, base_host, base_port) = split_url(base).ok()?;
    let default_port = if base_scheme == "https" { 443 } else { 80 };
    let authority = if base_port == default_port {
        base_host.clone()
    } else {
        format!("{base_host}:{base_port}")
    };
    let origin = format!("{base_scheme}://{authority}");

    if let Some(rest) = location.strip_prefix("//") {
        // Protocol-relative: the base's scheme, the Location's authority.
        return resolve_location(base, &format!("{base_scheme}://{rest}"));
    }
    if location.starts_with('/') {
        return Some(format!("{origin}{location}"));
    }
    // Relative to the base's directory. The base path is whatever follows the
    // authority, minus any query or fragment.
    let after_scheme = base.split_once("://").map(|(_, r)| r).unwrap_or("");
    let path = after_scheme
        .find('/')
        .map(|i| &after_scheme[i..])
        .unwrap_or("/");
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Some(format!("{origin}{dir}{location}"))
}

/// The lowercased scheme of an absolute URI reference, or `None` if it is
/// relative.
///
/// RFC 3986 §3.1: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`, and the
/// colon must come before any `/`, `?` or `#` — otherwise `a/b:c` would read
/// as scheme `a/b`.
fn leading_scheme(reference: &str) -> Option<String> {
    let end = reference.find(':')?;
    let scheme = &reference[..end];
    if reference[..end].contains(['/', '?', '#']) {
        return None;
    }
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
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
        // The message is the contract now; the host attaches its own code.
        assert!(e.contains("whole internet"), "{e}");
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
    fn refusal_names_the_destination() {
        let p = policy(&["https://api.github.com"]);
        let e = p.permits("https://evil.com/x").unwrap_err();
        assert!(matches!(e, EgressDenied::Host { .. }));
        assert!(e.to_string().contains("evil.com"), "{e}");
    }

    #[test]
    fn a_url_that_does_not_parse_is_refused_rather_than_allowed() {
        // Fail closed: an unparseable destination must not slip past a policy
        // because the parser could not decide what it was.
        let p = policy(&["https://api.github.com"]);
        assert!(p.permits("not-a-url").is_err());
        assert!(p.permits("").is_err());
    }

    // ---- redirect support (#99) --------------------------------------------

    #[test]
    fn private_destinations_are_recognized_in_every_literal_form() {
        for private in [
            "http://127.0.0.1:7461/api",
            "http://127.8.9.10/",           // whole /8, not just .1
            "http://localhost:8080/",
            "http://console.localhost/",    // RFC 6761 subdomains
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://192.168.1.1/admin",
            "http://0.0.0.0:80/",
            "http://[::1]:9000/",
            "http://[fd00::1]/",            // unique-local
            "http://[fe80::1]:80/",         // link-local
            "http://[::ffff:127.0.0.1]/",   // v4-mapped loopback
        ] {
            assert!(is_private_destination(private), "{private} must classify private");
        }
        for public in [
            "https://api.github.com/",
            "https://gmail.googleapis.com/gmail/v1/x",
            "http://93.184.216.34/",        // a public literal is not private
            "https://[2606:4700::6810:85e5]/",
        ] {
            assert!(!is_private_destination(public), "{public} must classify public");
        }
        // Fails closed on garbage: an unparseable destination never gets the
        // benefit of the doubt.
        assert!(is_private_destination("not-a-url"));
    }

    #[test]
    fn private_destinations_are_recognized_in_the_alternate_integer_encodings() {
        // The forms `Ipv4Addr::from_str` rejects but a libc resolver still maps
        // to loopback / the metadata service — every off-the-shelf SSRF payload
        // reaches for one of these first, so the deny must see them.
        for private in [
            "http://2130706433/",                  // 127.0.0.1, decimal
            "http://0x7f000001/",                   // 127.0.0.1, hex
            "http://017700000001/",                 // 127.0.0.1, octal
            "http://127.1/",                        // 127.0.0.1, short form
            "http://127.0.0x1/",                    // mixed-radix short form
            "http://0/",                            // 0.0.0.0, bare zero
            "http://2852039166/latest/meta-data/",  // 169.254.169.254 (IMDS), decimal
            "http://0xA9FEA9FE/",                   // 169.254.169.254, hex
            "http://127.0.0.1./",                   // trailing-dot FQDN
            "http://localhost./",                   // trailing-dot localhost
        ] {
            assert!(is_private_destination(private), "{private} must classify private");
        }
        // A genuine hostname and a public numeric literal still read as public —
        // the canonicalization must not over-match.
        for public in [
            "https://api.github.com/",
            "http://134744072/",                    // 8.8.8.8, decimal — public
            "http://999.999.999.999/",              // not a valid address at all
        ] {
            assert!(!is_private_destination(public), "{public} must classify public");
        }
    }

    #[test]
    fn same_origin_compares_scheme_host_and_port() {
        assert!(same_origin("https://api.example.com/a", "https://api.example.com/b?q=1"));
        assert!(same_origin("https://api.example.com/a", "https://API.Example.com:443/b"));
        assert!(!same_origin("https://api.example.com/a", "http://api.example.com/a"));
        assert!(!same_origin("https://api.example.com/a", "https://api.example.com:8443/a"));
        assert!(!same_origin("https://api.example.com/a", "https://other.example.com/a"));
        // The userinfo trick must not read as the same origin either.
        assert!(!same_origin("https://api.example.com/a", "https://api.example.com@evil.com/a"));
    }

    #[test]
    fn same_origin_fails_closed_on_an_unparseable_url() {
        // A credential must never be attached because the parser gave up.
        assert!(!same_origin("https://api.example.com/a", "not-a-url"));
        assert!(!same_origin("", ""));
    }

    #[test]
    fn a_location_resolves_in_all_four_shapes() {
        let base = "https://api.example.com/v1/messages?page=2";
        assert_eq!(
            resolve_location(base, "https://other.example.com/x").as_deref(),
            Some("https://other.example.com/x")
        );
        assert_eq!(
            resolve_location(base, "//other.example.com/x").as_deref(),
            Some("https://other.example.com/x"),
            "protocol-relative takes the base's scheme"
        );
        assert_eq!(
            resolve_location(base, "/x").as_deref(),
            Some("https://api.example.com/x"),
            "an absolute path takes the base's origin"
        );
        assert_eq!(
            resolve_location(base, "next").as_deref(),
            Some("https://api.example.com/v1/next"),
            "a relative path takes the base's DIRECTORY, and the query is dropped"
        );
    }

    #[test]
    fn a_non_default_port_survives_resolution() {
        assert_eq!(
            resolve_location("http://127.0.0.1:8080/a/b", "/c").as_deref(),
            Some("http://127.0.0.1:8080/c"),
            "resolving must not silently move the request to port 80"
        );
    }

    #[test]
    fn a_location_that_is_not_an_http_url_is_refused_rather_than_guessed_at() {
        let base = "https://api.example.com/v1/x";
        // Following any of these would send the request somewhere the
        // allowlist has no way to reason about, so none of them resolve.
        for bad in ["", "   ", "file:///etc/passwd", "javascript:alert(1)", "ftp://f.example/x"] {
            assert_eq!(resolve_location(base, bad), None, "{bad:?} must not resolve");
        }
    }

    #[test]
    fn a_location_carrying_a_control_character_is_refused() {
        // Response splitting: a `Location` with CRLF in it is an attempt to
        // author part of the next request, not a destination.
        let base = "https://api.example.com/v1/x";
        assert_eq!(resolve_location(base, "/a\r\nX-Injected: 1"), None);
        assert_eq!(resolve_location(base, "https://ok.example/\u{0}"), None);
    }
}
