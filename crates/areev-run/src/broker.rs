//! The credential broker — a loopback service that makes outbound calls on a
//! connector's behalf so the connector never holds the token.
//!
//! Posta's CB4A calls this Model A: *"The safest credential for an AI agent is
//! one it never holds."* The connector calls us unauthenticated over loopback,
//! we check the destination against its allowlist, attach credentials, and make
//! the real request. Cloudflare shipped exactly this in April 2026 ("no token
//! is ever passed into the sandbox"); Deno in February; it is the whole of
//! Nango's product.
//!
//! ## The shape, and its honest cost
//!
//! This is a **reverse** broker, not a forward proxy, and the difference
//! matters. A forward proxy cannot inject an `Authorization` header into an
//! HTTPS request without terminating TLS, which means shipping a CA the
//! connector trusts — a much larger and more dangerous mechanism. So instead the
//! connector posts us a *description* of the call it wants:
//!
//! ```json
//! { "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
//!   "method": "GET", "credential": "gmail",
//!   "headers": { "X-Goog-User-Project": "my-project" } }
//! ```
//!
//! and we answer with the response. The cost is real and worth stating: a
//! connector written this way cannot use a vendor SDK, because the SDK wants to
//! make its own sockets. That is the same trade Nango makes.
//!
//! ## What the connector gets instead of a secret
//!
//! `AREEV_EGRESS_URL` in its environment. Nothing else. The credential values
//! stay in this process, read from host-named environment variables, and never
//! appear in a grain — a declaration names a credential, it never carries one.
//!
//! ## Bind and reach
//!
//! Loopback only, on an ephemeral port, for the lifetime of one evaluation
//! pass. It is not a server anyone deploys, it has no configuration file, and
//! it does not outlive the command — consistent with a product whose stance is
//! that nothing stays resident.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use areev_cal::AreevFacade;
use areev_core::types::capability::Declaration;

use crate::egress::{EgressDenied, EgressPolicy};

/// Config errors are plain messages; each host wraps them in its own error.
type Result<T> = std::result::Result<T, String>;

/// Largest request body the broker will read, mirroring the console's cap.
///
/// This bounds the JSON a CALLER sends the broker, never an artifact: a
/// `body_ref` upload and an artifact-mode response cross this boundary as a
/// `cas://` address, and are bounded by [`CapabilityLimits`] instead (#339).
const MAX_BODY: usize = 1024 * 1024;

/// Ceiling on one brokered artifact transfer when nothing is declared (#339).
const DEFAULT_TRANSFER: usize = areev_core::types::capability::DEFAULT_TRANSFER_BYTES as usize;
/// The hard maximum a declaration may raise it to (#339): 32 MiB.
const MAX_TRANSFER: usize = areev_core::types::capability::MAX_TRANSFER_BYTES as usize;

/// How a credential is attached to an outbound request.
///
/// This is the RESOLVED value. Where it came from is [`CredentialSource`],
/// which may mint a fresh one per call.
#[derive(Clone)]
pub enum Credential {
    /// `Authorization: Bearer <value>`
    Bearer(String),
    /// A named header carrying the value verbatim.
    Header { name: String, value: String },
}

/// Redacted, deliberately (#113).
///
/// A derived `Debug` puts the secret itself into any error chain, panic
/// message, or `{:?}` a host reaches for while debugging — which is how a
/// credential ends up in a log file that outlives the process holding it. The
/// same reasoning already redacts `executor_uri` (SR-F5). The variant and the
/// header NAME survive, because those are what a reader actually needs.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Bearer(_) => f.write_str("Bearer([redacted])"),
            Credential::Header { name, .. } => {
                write!(f, "Header {{ name: {name:?}, value: [redacted] }}")
            }
        }
    }
}

impl Credential {
    /// Resolve from an environment variable **name**, never a literal.
    ///
    /// The host names the variable, the same discipline `--passphrase-env` and
    /// `--token-env` use: a value that appears on a command line is a value in
    /// shell history and in `ps` output.
    ///
    /// ## Reading the name is also what registers it as a secret
    ///
    /// Withholding happens HERE rather than at each host's flag-parsing site,
    /// and that placement is the fix for #100. Until 1.6.0 the withhold list
    /// was three flags long (`--passphrase-env`, `--token-env`,
    /// `--anon-key-env`, registered in the CLI's one choke point) and
    /// `--credential NAME=VAR` was not on it — so the raw credential stayed in
    /// the inherited environment of every tool, connector and sandbox
    /// subprocess. A tool could read the secret straight out of its own
    /// environment and never call the broker at all, which is the precise
    /// opposite of what brokering is for.
    ///
    /// Four hosts read credentials this way — `areev run`'s `--credential`,
    /// `areev trigger run`'s, and the Python and Node bindings — so a fix at
    /// any one call site would have left the other three open. Registering as
    /// a side effect of *reading the name* is what makes it structural: a
    /// credential this process can resolve is a credential its children cannot
    /// see. The child still receives `AREEV_EGRESS_URL` + `AREEV_EGRESS_TOKEN`,
    /// which are applied AFTER the environment policy (`proc::run`), so the
    /// broker handshake is unaffected.
    pub fn bearer_from_env(var: &str) -> Result<Credential> {
        let v = std::env::var(var)
            .map_err(|_| format!("credential env var {var} is not set"))?;
        if v.trim().is_empty() {
            return Err(format!("credential env var {var} is empty"));
        }
        areev_core::proc::deny_env_var(var);
        Ok(Credential::Bearer(v))
    }

    /// Parse `ENV_VAR` or `ENV_VAR@principal` — the spec form `--credential
    /// NAME=…` takes — returning the credential and its owner, if bound.
    ///
    /// The owner is the **run principal this credential belongs to**. A bound
    /// credential is refused for any run executing as anyone else — including
    /// a run with no principal bound at all, which fails closed (see
    /// [`Broker::bind_credential_owner`]). `@` is safe as the separator
    /// because an environment variable name cannot contain one, and a
    /// principal (`user:alice`) can contain `:`, which rules the natural
    /// alternative out.
    pub fn bearer_from_env_spec(spec: &str) -> Result<(Credential, Option<String>)> {
        let (var, owner) = match spec.split_once('@') {
            Some((v, o)) if !o.trim().is_empty() => (v.trim(), Some(o.trim().to_string())),
            // `VAR@` with an empty principal is a typo, or an unset shell
            // variable expanded into the owner position (`VAR@$OWNER` with
            // $OWNER unset) — NOT a request for an unbound credential. Treating
            // it as unbound fails OPEN: the confinement the operator spelled
            // out silently disappears. Refuse it instead; an unbound credential
            // is spelled `NAME=VAR`, with no `@` at all.
            Some((_, _)) => {
                return Err(format!(
                    "credential spec {spec:?} has an empty principal after '@' — write \
                     NAME=VAR for an unbound credential, or NAME=VAR@principal to bind one"
                ))
            }
            None => (spec.trim(), None),
        };
        Ok((Self::bearer_from_env(var)?, owner))
    }
}

/// How long a minted credential may be reused before it is minted again.
///
/// 300s: short enough that a rotation or revocation upstream takes effect
/// within a superstep or two, long enough that a plan making a call per node
/// does not spawn a resolver per call. A cloud access token's own lifetime is
/// typically an hour, so this is well inside it.
pub const DEFAULT_CREDENTIAL_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Wall-clock ceiling on one resolver. A vault round-trip is milliseconds and
/// a cloud CLI's token mint is a second or two; past this something is wedged,
/// and the call it was for should fail rather than hold a superstep open.
const RESOLVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A credential is a token, not a payload. 64 KiB is orders of magnitude above
/// any real one and still bounds a resolver that decides to print a file.
const RESOLVER_MAX_OUTPUT: usize = 64 * 1024;

/// Where a credential's value comes from (#113).
///
/// ## Why the source is a seam
///
/// Until 1.6.1 a brokered credential could only be an environment variable,
/// which made every brokered secret **static for the life of the process** —
/// the wrong shape for the credentials capability tools actually use. A Google
/// access token expires roughly hourly, so an unattended heartbeat needed a
/// refresh step outside Areev that it could silently get wrong; a run parked
/// on a human gate for a day resumed with yesterday's token; and a long-lived
/// secret sitting in an environment is the very leak surface #100 narrowed.
/// Vault and secret-manager users have it sharper still: their whole model is
/// short TTLs and central revocation, and an env var defeats both.
///
/// So the source became a seam, resolved by the BROKER at call time — the same
/// subprocess-seam pattern `--tool-cmd`, `--embed-cmd` and `--llm-cmd` already
/// use. What the guest sees does not change at all: it names a credential by
/// label and holds nothing, exactly as before.
///
/// ## Why the resolver runs here and not in the sandbox
///
/// The same reasoning #106 used to route blob reads through the broker: the
/// trusted party performs the privileged act, so it is also the party that can
/// record it, bound it, and fail it closed. A resolver in the guest would mean
/// handing the guest the vault's own credential — one class *worse* than the
/// static token this replaces.
#[derive(Debug, Clone)]
pub enum CredentialSource {
    /// Read once, from an environment variable, at configuration time. The
    /// pre-1.6.2 behaviour and still the default: no subprocess, no cache, no
    /// failure mode at call time.
    Static(Credential),
    /// Minted by running a command and taking its stdout (#113).
    ///
    /// The one that subsumes the rest — `vault kv get`, `gcloud auth
    /// print-access-token`, `aws secretsmanager get-secret-value` — with no
    /// vendor client in the dependency graph. Never put a literal secret in
    /// the command: it is host configuration and is printable as such.
    Command {
        command: String,
        ttl: std::time::Duration,
        /// Variables passed through to the resolver, and ONLY to it — the
        /// resolver's own auth (`VAULT_TOKEN`, `AWS_PROFILE`, …). See
        /// [`CredentialSource::spawn_policy`] for why this list exists at all.
        pass_env: Vec<String>,
    },
    /// Read from HashiCorp Vault or OpenBao's KV API over its HTTP interface.
    ///
    /// A convenience over the same interface `Command` offers, and worth
    /// having natively for one concrete reason: a container that resolves this
    /// way needs no `vault` binary in the image. `VAULT_ADDR` and
    /// `VAULT_TOKEN` (plus optional `VAULT_NAMESPACE`) come from this
    /// process's environment and are registered as secrets, so no child sees
    /// them.
    Vault {
        /// The API path verbatim, including KV v2's `data/` segment:
        /// `secret/data/google`. Not synthesised, because guessing a mount's
        /// version for the operator is how a working path becomes a 404.
        path: String,
        /// Which field of the secret carries the token.
        field: String,
        ttl: std::time::Duration,
    },
}

impl From<Credential> for CredentialSource {
    fn from(c: Credential) -> Self {
        CredentialSource::Static(c)
    }
}

impl CredentialSource {
    /// Parse the value half of a `--credential NAME=<spec>` pair.
    ///
    /// Three forms, discriminated by prefix, with the bare form unchanged:
    ///
    /// | Spec | Source |
    /// |---|---|
    /// | `SHEETS_TOKEN` / `SHEETS_TOKEN@user:alice` | environment variable |
    /// | `cmd:gcloud auth print-access-token` | [`CredentialSource::Command`] |
    /// | `vault:secret/data/google#access_token` | [`CredentialSource::Vault`] |
    ///
    /// ## Why only the bare form parses `@principal`
    ///
    /// An environment variable name cannot contain `@`, so `VAR@user:alice`
    /// splits unambiguously — that is the 1.6.0 grammar and it keeps working.
    /// A **command** can contain `@` anywhere (`curl -u svc@example.com`), so
    /// splitting one on `@` would silently re-read part of the command as a
    /// principal and bind the credential to the wrong owner. Getting that
    /// wrong is not cosmetic: the owner is what stops one principal's run
    /// spending another's secret. So for `cmd:`/`vault:` the whole remainder
    /// is the source, and a principal is named on the NAME side instead —
    /// `--credential 'sheets@user:alice=cmd:…'`, where the name is ours and
    /// carries no such ambiguity.
    pub fn from_spec(spec: &str) -> Result<(CredentialSource, Option<String>)> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("cmd:") {
            let command = rest.trim();
            if command.is_empty() {
                return Err("credential spec 'cmd:' names no command".into());
            }
            return Ok((
                CredentialSource::Command {
                    command: command.to_string(),
                    ttl: DEFAULT_CREDENTIAL_TTL,
                    pass_env: Vec::new(),
                },
                None,
            ));
        }
        if let Some(rest) = spec.strip_prefix("vault:") {
            let (path, field) = rest.trim().split_once('#').ok_or_else(|| {
                format!(
                    "credential spec {spec:?}: a vault source is written \
                     vault:<path>#<field>, e.g. vault:secret/data/google#access_token"
                )
            })?;
            let (path, field) = (path.trim(), field.trim());
            if path.is_empty() || field.is_empty() {
                return Err(format!(
                    "credential spec {spec:?}: both the path and the field must be non-empty"
                ));
            }
            // Reading the names is what registers them, exactly as
            // `bearer_from_env` does (#100) — a credential this process can
            // resolve must be one its children cannot.
            for var in ["VAULT_TOKEN", "VAULT_ADDR", "VAULT_NAMESPACE"] {
                areev_core::proc::deny_env_var(var);
            }
            return Ok((
                CredentialSource::Vault {
                    path: path.to_string(),
                    field: field.to_string(),
                    ttl: DEFAULT_CREDENTIAL_TTL,
                },
                None,
            ));
        }
        let (cred, owner) = Credential::bearer_from_env_spec(spec)?;
        Ok((CredentialSource::Static(cred), owner))
    }

    /// Apply host-level resolver settings. No-op on a static source, which has
    /// nothing to mint and nothing to cache.
    pub fn with_resolver_config(
        mut self,
        ttl_secs: Option<u64>,
        resolver_env: &[String],
    ) -> CredentialSource {
        match &mut self {
            CredentialSource::Static(_) => {}
            CredentialSource::Command { ttl, pass_env, .. } => {
                if let Some(s) = ttl_secs {
                    // Clamped so `Instant + ttl` cannot overflow downstream.
                    // ~136 years is past any honest intent and short of the
                    // panic.
                    *ttl = std::time::Duration::from_secs(s.min(u64::from(u32::MAX)));
                }
                pass_env.extend(resolver_env.iter().cloned());
            }
            CredentialSource::Vault { ttl, .. } => {
                if let Some(s) = ttl_secs {
                    // Clamped so `Instant + ttl` cannot overflow downstream.
                    // ~136 years is past any honest intent and short of the
                    // panic.
                    *ttl = std::time::Duration::from_secs(s.min(u64::from(u32::MAX)));
                }
            }
        }
        self
    }

    /// Is this source re-mintable — i.e. worth invalidating and retrying when
    /// an upstream answers 401?
    fn is_dynamic(&self) -> bool {
        !matches!(self, CredentialSource::Static(_))
    }

    fn ttl(&self) -> std::time::Duration {
        match self {
            CredentialSource::Static(_) => std::time::Duration::ZERO,
            CredentialSource::Command { ttl, .. } | CredentialSource::Vault { ttl, .. } => *ttl,
        }
    }

    /// The policy a resolver subprocess runs under.
    ///
    /// `ClearExcept`, not the `InheritExcept` the other seams use, and that
    /// asymmetry is the whole point. The resolver needs its OWN credential —
    /// `VAULT_TOKEN`, an AWS profile, a service-account path — which is a
    /// secret one class more powerful than the one it fetches: it can fetch
    /// all of them. Under `InheritExcept` that variable would sit in the
    /// environment of every `--tool-cmd` subprocess too, which is exactly the
    /// leak #100 closed for the credential itself, reopened one level up.
    ///
    /// So the operator names those variables (`--resolver-env`), they are
    /// registered as secrets so ordinary children never see them, and they are
    /// re-admitted HERE, for resolvers only. A resolver that names nothing
    /// gets `PATH`/`HOME` and little else — enough for `gcloud` or `aws` to
    /// find their own config, and nothing ambient beyond it.
    fn spawn_policy(pass_env: &[String]) -> areev_core::proc::SpawnPolicy {
        let mut allow = areev_core::proc::EnvPolicy::minimal_allow();
        allow.extend(pass_env.iter().cloned());
        areev_core::proc::SpawnPolicy {
            timeout: Some(RESOLVER_TIMEOUT),
            max_output_bytes: RESOLVER_MAX_OUTPUT,
            env: areev_core::proc::EnvPolicy::ClearExcept { allow },
            stderr: areev_core::proc::StderrMode::Pipe,
            current_dir: None,
        }
    }
}

/// Why a grant refused a credential (#112).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialDenied {
    /// The caller was never granted this credential under any host.
    NotGranted,
    /// It holds the credential, but not for this destination.
    WrongHost,
}

/// What one caller may do through the broker.
///
/// Deny by default in both directions: a caller with no grant may do nothing,
/// and a grant that names no methods may only read. Connectors read; tools
/// write, and the write verb is exactly the one worth making deliberate.
#[derive(Debug, Clone, Default)]
pub struct CallerGrant {
    /// Credential names this caller may ask for, each optionally narrowed to
    /// the hosts it may be sent TO (#112). Absent name = not granted; an
    /// empty host list = any host the rest of the chain already permits, which
    /// is what an unpaired `--tool-egress 'tool:cred:POST'` means and what
    /// every grant meant before the pairing existed.
    ///
    /// Private, unlike the `pub` set it replaces, because the invariant is now
    /// "a name and its hosts are decided together": a caller reaching in to
    /// add a bare name could silently widen a pairing an operator wrote.
    credentials: BTreeMap<String, Vec<areev_core::types::capability::AllowedHost>>,
    /// Methods it may issue. Empty = `GET`/`HEAD` only.
    methods: std::collections::BTreeSet<String>,
}

impl CallerGrant {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant a credential for any host the allowlist and declaration permit.
    pub fn credential(mut self, name: &str) -> Self {
        self.credentials.entry(name.to_string()).or_default();
        self
    }

    /// Grant a credential ONLY for these hosts (#112).
    ///
    /// The host-side half of the pairing. Without it, the declaration alone
    /// carries the credential↔host binding — and a declaration is the half
    /// that arrives with the tool, so a compromised or simply wrong one could
    /// pair a credential with a host the operator never intended.
    ///
    /// Repeated calls accumulate hosts. Mixing this with
    /// [`CallerGrant::credential`] for one name — `gmail@a.example+gmail`, an
    /// operator contradicting themselves — leaves the name PAIRED whichever
    /// order the two arrive in: `credential` never clears an existing host
    /// list, and `credential_for` narrows an empty one. There is deliberately
    /// no way to widen a pairing back, because the alternative is a
    /// restriction that silently disappears depending on argument order.
    pub fn credential_for(
        mut self,
        name: &str,
        hosts: Vec<areev_core::types::capability::AllowedHost>,
    ) -> Self {
        self.credentials.entry(name.to_string()).or_default().extend(hosts);
        self
    }

    pub fn method(mut self, m: &str) -> Self {
        self.methods.insert(m.trim().to_ascii_uppercase());
        self
    }

    fn permits_method(&self, method: &str) -> bool {
        if self.methods.is_empty() {
            return matches!(method, "GET" | "HEAD");
        }
        self.methods.contains(method)
    }

    /// May this caller spend `name` on a request to `url` (#112)?
    ///
    /// Checked once, at dispatch entry, and deliberately not per redirect hop:
    /// the credential rides a hop only while the chain has never left its
    /// starting origin (`dispatch`'s `left_origin` latch), so the destination
    /// this pairing judged is the only destination the secret can reach. If
    /// that latch ever loosens, this has to move into the hop loop with it.
    fn permits_credential(&self, name: &str, url: &str) -> std::result::Result<(), CredentialDenied> {
        let Some(hosts) = self.credentials.get(name) else {
            return Err(CredentialDenied::NotGranted);
        };
        if hosts.is_empty() || hosts.iter().any(|h| h.permits_url(url)) {
            return Ok(());
        }
        Err(CredentialDenied::WrongHost)
    }
}

/// Who may do what, keyed by caller name (a tool name, or a connector name).
///
/// ## Why the grant is host config and not a grain
///
/// A `Tool` Definition declaring "I may reach api.example.com with credential
/// X" would be a permission arriving in the same bundle as the code it
/// authorizes — the exact thing [`crate::executor::CodeExecutor`] refuses for
/// code. The tool says which credential it wants *at call time*, by name; the
/// host decides whether it may have it. Intent travels; authority does not.
#[derive(Debug, Clone, Default)]
pub struct EgressGrants {
    by_caller: BTreeMap<String, CallerGrant>,
    /// Applied to a caller with no entry of its own. `None` = such a caller
    /// gets nothing. The connector path sets this, because one connector runs
    /// per pass and there is no second caller to tell it apart from.
    default_grant: Option<CallerGrant>,
}

impl EgressGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant `caller` (a tool name) a specific scope.
    pub fn grant(mut self, caller: &str, g: CallerGrant) -> Self {
        self.by_caller.insert(caller.to_string(), g);
        self
    }

    /// Give every unlisted caller this scope. Spelled out so it cannot be
    /// reached by writing `default()`.
    pub fn default_for_all(mut self, g: CallerGrant) -> Self {
        self.default_grant = Some(g);
        self
    }

    fn for_caller(&self, caller: &str) -> Option<&CallerGrant> {
        self.by_caller.get(caller).or(self.default_grant.as_ref())
    }

    fn callers(&self) -> Vec<String> {
        self.by_caller.keys().cloned().collect()
    }
}

/// One credential minted and held until it expires.
struct CachedCredential {
    value: Credential,
    expires_at: std::time::Instant,
}

/// The resolved-credential cache: name -> what was minted and when it lapses.
type CredentialCache = Arc<std::sync::Mutex<BTreeMap<String, CachedCredential>>>;

/// Resolve `name` to a value, minting it if there is nothing fresh cached.
///
/// ## Why this is cached at all
///
/// Resolving per HTTP call would spawn a process per call — a plan that pages
/// through an API would fork a `gcloud` per page. The TTL is the compromise,
/// and it is short by default so that revoking a secret upstream takes effect
/// here without anyone restarting anything.
///
/// ## Fail closed, and say only which credential failed
///
/// A resolver that errors, times out, or returns nothing must refuse the call.
/// Falling through to an unauthenticated request would produce a 401 from
/// someone else's API hours later — the exact failure mode this feature exists
/// to remove. And the error names the CREDENTIAL, never the resolver's output:
/// stdout is by definition the secret, and stderr is written by a script that
/// may have echoed it. An operator who wants their resolver's diagnostics
/// redirects them inside their own command.
fn resolve_credential(
    name: &str,
    source: &CredentialSource,
    cache: &CredentialCache,
) -> std::result::Result<Credential, String> {
    if let CredentialSource::Static(c) = source {
        return Ok(c.clone());
    }
    let now = std::time::Instant::now();
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(name) {
            if hit.expires_at > now {
                return Ok(hit.value.clone());
            }
        }
    }
    let value = match source {
        CredentialSource::Static(_) => unreachable!("handled above"),
        CredentialSource::Command { command, pass_env, .. } => {
            mint_from_command(name, command, pass_env)?
        }
        CredentialSource::Vault { path, field, .. } => mint_from_vault(name, path, field)?,
    };
    let token = Credential::Bearer(value);
    if let Ok(mut guard) = cache.lock() {
        // `Instant + Duration` PANICS on overflow, and this runs inside the
        // cache lock on the broker's accept-loop thread — an operator typo of
        // an absurd `--credential-ttl` would take the whole broker down, not
        // just this call. Falling back to `now` means "already expired", which
        // degrades to minting per call: slow, never wrong, never fatal. The
        // CLI also clamps, so this is the belt under those braces.
        let expires_at = now.checked_add(source.ttl()).unwrap_or(now);
        guard.insert(
            name.to_string(),
            CachedCredential { value: token.clone(), expires_at },
        );
    }
    Ok(token)
}

/// Drop a cached value so the next call mints a new one.
fn invalidate_credential(name: &str, cache: &CredentialCache) {
    if let Ok(mut guard) = cache.lock() {
        guard.remove(name);
    }
}

/// stdout of a resolver command, trimmed, as the credential value.
fn mint_from_command(
    name: &str,
    command: &str,
    pass_env: &[String],
) -> std::result::Result<String, String> {
    use std::process::Command;
    // The platform shell: /bin/sh -c on unix, cmd /C on Windows. The Windows
    // command string must go through raw_arg — Command::arg MSVC-quotes
    // embedded quotes, which cmd.exe does not parse.
    #[cfg(not(windows))]
    let mut shell = Command::new("/bin/sh");
    #[cfg(not(windows))]
    shell.arg("-c").arg(command);
    #[cfg(windows)]
    let mut shell = Command::new("cmd");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        shell.raw_arg("/C").raw_arg(command);
    }
    let policy = CredentialSource::spawn_policy(pass_env);
    let out = areev_core::proc::run(shell, None, &[], &policy)
        .map_err(|e| format!("credential '{name}': its resolver could not be started ({e})"))?;
    if out.timed_out {
        return Err(format!(
            "credential '{name}': its resolver exceeded {}s and was killed",
            RESOLVER_TIMEOUT.as_secs()
        ));
    }
    if !out.status.success() {
        // Deliberately without stderr — see this module's fail-closed note.
        return Err(format!(
            "credential '{name}': its resolver exited with {}",
            out.status
        ));
    }
    validate_minted(name, String::from_utf8_lossy(&out.stdout).trim())
}

/// A minted value has to survive the same scrutiny a declared header does.
///
/// A resolver that returns a value containing CR or LF would let whatever
/// wrote it author a second header on every request the credential rides —
/// header injection sourced from the one input this subsystem was otherwise
/// treating as trusted. Empty is refused for a duller reason: a script that
/// prints nothing on failure is common, and an empty bearer token is an
/// unauthenticated request wearing an `Authorization` header.
fn validate_minted(name: &str, value: &str) -> std::result::Result<String, String> {
    if value.is_empty() {
        return Err(format!("credential '{name}': its resolver returned nothing"));
    }
    if !areev_core::types::capability::is_valid_header_value(value) {
        return Err(format!(
            "credential '{name}': its resolver returned a value that is not a valid HTTP header \
             value — a control character here would forge a second header on every request"
        ));
    }
    Ok(value.to_string())
}

/// Read one field of a KV secret from Vault or OpenBao.
///
/// Both KV versions are tried against the SAME path the operator wrote: v2
/// nests the secret under `data.data`, v1 puts it at `data`. Trying both is
/// not guesswork about the mount — the path is verbatim either way — it just
/// spares an operator from having to know which shape their mount returns.
fn mint_from_vault(name: &str, path: &str, field: &str) -> std::result::Result<String, String> {
    let missing = |var: &str| {
        format!("credential '{name}': a vault source needs {var} in this process's environment")
    };
    let addr = std::env::var("VAULT_ADDR").map_err(|_| missing("VAULT_ADDR"))?;
    let token = std::env::var("VAULT_TOKEN").map_err(|_| missing("VAULT_TOKEN"))?;
    let url = format!("{}/v1/{}", addr.trim_end_matches('/'), path.trim_start_matches('/'));
    // Every leg is bounded, including the BODY read. ureq's default leaves
    // `recv_body` unset, and this runs inline on the broker's accept loop — so
    // a `$VAULT_ADDR` that answers headers promptly and then drips the body
    // would stop the broker accepting anything at all, hanging every tool in
    // the run rather than failing this one call. `RESOLVER_TIMEOUT` promises
    // the opposite, so it has to cover the whole exchange.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(RESOLVER_TIMEOUT))
        .timeout_recv_body(Some(RESOLVER_TIMEOUT))
        .timeout_global(Some(RESOLVER_TIMEOUT))
        .build()
        .into();
    let mut req = agent.get(&url).header("X-Vault-Token", &token);
    if let Ok(ns) = std::env::var("VAULT_NAMESPACE") {
        req = req.header("X-Vault-Namespace", &ns);
    }
    // The status and the path are safe to report — neither is the secret —
    // and without them a misconfigured mount is undiagnosable.
    let mut resp = req
        .call()
        .map_err(|e| format!("credential '{name}': vault at {url} did not answer ({e})"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!(
            "credential '{name}': vault answered {status} for {path} — check the path, the \
             token's policy, and (for KV v2) that the path includes its 'data/' segment"
        ));
    }
    // Read then parse with serde_json rather than ureq's own `read_json`,
    // which would mean turning on a ureq feature for a single call site.
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|_| format!("credential '{name}': vault's answer could not be read"))?;
    let body: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| format!("credential '{name}': vault's answer was not JSON"))?;
    let found = body
        .pointer("/data/data")
        .and_then(|d| d.get(field))
        .or_else(|| body.pointer("/data").and_then(|d| d.get(field)))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("credential '{name}': vault's secret at {path} has no string field '{field}'")
        })?;
    validate_minted(name, found.trim())
}

/// An unguessable per-caller capability token.
///
/// The broker binds to loopback, and loopback is not an authorization: any
/// process on the box could otherwise post to it and spend the credentials it
/// holds. The token also makes per-caller scoping possible at all — without
/// it the broker cannot tell which tool is calling, and N pool workers share
/// one port.
fn mint_token() -> String {
    let mut b = [0u8; 24];
    // A broker that cannot get randomness must not fall back to something
    // guessable; the caller turns this into a refusal to start.
    getrandom::fill(&mut b).expect("OS randomness for the egress token");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One refused outbound call, kept for the audit record.
///
/// A refusal is an agent reaching for somewhere it was not allowed — the
/// single most audit-worthy event this subsystem produces, and the one a
/// reviewer asks about ("did it ever try?"). stderr answers that only until
/// the terminal scrolls, so the driver journals these into the memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRefusal {
    /// The tool or connector that asked. Empty for a caller with no name of
    /// its own (the connector path's default grant).
    pub caller: String,
    /// Where it tried to go, as the caller spelled it.
    pub destination: String,
    /// Why it was refused, in one phrase.
    pub reason: String,
}

/// One call that WENT OUT, kept for the audit trail (#101).
///
/// A capability tool's whole bargain is that its I/O is mediated and
/// recorded, so the successful calls matter as much as the refused ones: "it
/// was allowed to reach Gmail" is a policy statement, "it sent these four
/// requests" is the evidence. Bodies are recorded as **digests**, never
/// contents — the journal is an immutable, replicating grain and a mailbox
/// body does not belong in one.
///
/// Deliberately NOT a journal entry, for the same reason [`EgressRefusal`] is
/// not: replay never sees it, so `verify` stays byte-identical whether or not
/// a broker was configured. It is evidence about the run, not a step of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressCall {
    /// The tool that asked.
    pub caller: String,
    pub method: String,
    /// The URL as dispatched. When a redirect was followed this is the FINAL
    /// URL, and `redirects` says how many hops it took to get there.
    pub url: String,
    pub status: u16,
    pub redirects: u32,
    /// `sha256:<hex>` of the request body, or `None` when there was none.
    pub request_digest: Option<String>,
    /// `sha256:<hex>` of the response body.
    pub response_digest: String,
    pub response_bytes: usize,
    /// Bounded media type of a byte-mode response, if requested.
    pub response_mime: Option<String>,
    /// Content address of the downloaded response, when stored as an artifact.
    pub response_ref: Option<String>,
    /// The credential NAME that was attached, never a value.
    pub credential: Option<String>,
    /// Non-credential request headers the caller set (#105), name AND value.
    ///
    /// Recorded in full, unlike the credential and unlike bodies: the caller
    /// supplied these, so they carry nothing the caller did not already know,
    /// and "it sent these four requests with these headers" is strictly more
    /// evidence than "it sent these four requests".
    pub headers: BTreeMap<String, String>,
}

/// One CAS blob a capability tool READ, kept for the audit trail (#106).
///
/// The mirror of [`EgressCall`] on the other mediated door. A `wasm32-areev-io`
/// module has no file descriptor of its own any more than it has a socket, so
/// every stored byte it opens comes through the broker and lands here: "it was
/// allowed to read attachments" is a policy statement, "it opened these two"
/// is the evidence.
///
/// The address IS the content, so recording it is recording exactly which
/// bytes were read, with no risk of putting the bytes themselves into an
/// immutable replicating grain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRead {
    /// The tool that asked.
    pub caller: String,
    /// The `cas://sha256:…` address it opened.
    pub uri: String,
    /// How many bytes it got.
    pub bytes: usize,
}

/// Per-caller ceilings on a capability tool's mediated egress.
///
/// Extism's model: overruns are typed errors, never truncation — a tool that
/// silently received half a response would produce a wrong answer with no
/// evidence that anything went wrong.
///
/// The two byte ceilings default to 1 MiB and may be declared up to the
/// 32 MiB hard maximum (`areev_core::types::capability::MAX_TRANSFER_BYTES`,
/// #339). The executor refuses a declaration outside `1..=32 MiB` before the
/// module runs; the broker re-checks an artifact transfer against the same
/// range before any upstream I/O, so a host constructing these directly
/// cannot get a silent clamp either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityLimits {
    /// Calls one caller may make for the life of the broker.
    pub max_calls: u32,
    /// Largest response body handed back to a caller — the text body, or the
    /// exact bytes an artifact-mode call stores in CAS.
    pub max_response_bytes: usize,
    /// Largest `body_ref` artifact a caller may upload (#339). Checked against
    /// the stored blob's size before the broker connects upstream, so an
    /// overrun is refused before the upstream sees a byte.
    pub max_request_bytes: usize,
}

impl Default for CapabilityLimits {
    fn default() -> Self {
        CapabilityLimits {
            max_calls: 64,
            max_response_bytes: DEFAULT_TRANSFER,
            max_request_bytes: DEFAULT_TRANSFER,
        }
    }
}

/// What a caller declared, and what it may spend.
#[derive(Debug, Clone, Default)]
struct Declared {
    declaration: areev_core::types::capability::Declaration,
    limits: CapabilityLimits,
    calls_made: u32,
}

/// A running broker. Dropping it stops the listener.
pub struct Broker {
    url: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Destinations refused during this pass, for journalling.
    refusals: Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
    /// Calls that succeeded, for journalling (#101).
    calls: Arc<std::sync::Mutex<Vec<EgressCall>>>,
    /// caller -> what its Definition declared. A caller with no entry is a
    /// subprocess tool or connector, which declares nothing and is governed
    /// by the host grant alone — today's behaviour, unchanged.
    declared: Arc<std::sync::Mutex<BTreeMap<String, Declared>>>,
    /// credential name -> the run principal it belongs to. A credential with
    /// no entry is spendable by any run the grant admits — today's behaviour.
    credential_owners: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    /// The principal the CURRENT run executes as, bound by the driver at
    /// drive entry. `None` until a run binds one — and an owned credential
    /// refuses under `None`, so a path that never binds (the trigger
    /// evaluator's connector pass, a bare library embedding) fails closed
    /// rather than open.
    run_principal: Arc<std::sync::Mutex<Option<String>>>,
    /// The memory whose CAS blobs `POST /blob` serves, if the host wired one
    /// (#106). `None` means blob reads are refused whatever a module declared
    /// — the same posture as a capability module under a host that configured
    /// no broker at all.
    blobs: Arc<std::sync::Mutex<Option<String>>>,
    /// Run-owned handle: only the driver can open the memory; the broker can
    /// write an artifact through this shared facade while workers execute.
    artifact_store: Arc<std::sync::Mutex<Option<Arc<AreevFacade>>>>,
    /// Every blob a caller actually read.
    blob_reads: Arc<std::sync::Mutex<Vec<BlobRead>>>,
    /// caller -> its capability token.
    tokens: BTreeMap<String, String>,
    /// The token an unlisted caller presents, when a default grant exists.
    default_token: Option<String>,
}

impl Broker {
    /// Start a broker on an ephemeral loopback port.
    ///
    /// Takes SOURCES rather than resolved values (#113). A static credential
    /// is one — `Credential` converts with `.into()` — so a host that reads an
    /// environment variable and one that mints per call reach the same
    /// constructor, and there is no second start path to drift from this one.
    pub fn start(
        policy: EgressPolicy,
        credentials: BTreeMap<String, CredentialSource>,
        grants: EgressGrants,
        refusal_code: &'static str,
    ) -> Result<Broker> {
        // 127.0.0.1 explicitly, never 0.0.0.0: a broker that holds credentials
        // and is reachable off-box is a credential server.
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("egress broker cannot bind loopback: {e}"))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        listener.set_nonblocking(true).ok();

        let stop = Arc::new(AtomicBool::new(false));
        let refusals = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let declared: Arc<std::sync::Mutex<BTreeMap<String, Declared>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let credential_owners: Arc<std::sync::Mutex<BTreeMap<String, String>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let run_principal: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let blobs: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let artifact_store = Arc::new(std::sync::Mutex::new(None));
        let blob_reads: Arc<std::sync::Mutex<Vec<BlobRead>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop_t, refusals_t) = (Arc::clone(&stop), Arc::clone(&refusals));
        let (calls_t, declared_t) = (Arc::clone(&calls), Arc::clone(&declared));
        let (owners_t, principal_t) = (Arc::clone(&credential_owners), Arc::clone(&run_principal));
        let (blobs_t, blob_reads_t) = (Arc::clone(&blobs), Arc::clone(&blob_reads));
        let artifact_store_t = Arc::clone(&artifact_store);
        // Minted credentials live for a TTL and no longer (#113). Held by the
        // BROKER rather than by each source so one invalidation — the 401
        // path — has a single place to reach.
        let cache: CredentialCache = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let cache_t = Arc::clone(&cache);

        // One token per caller, minted before the listener serves anything.
        let mut tokens = BTreeMap::new();
        for c in grants.callers() {
            tokens.insert(c, mint_token());
        }
        let default_token = grants.default_grant.as_ref().map(|_| mint_token());
        let by_token: BTreeMap<String, String> = tokens
            .iter()
            .map(|(c, t)| (t.clone(), c.clone()))
            .chain(default_token.iter().map(|t| (t.clone(), String::new())))
            .collect();

        let handle = std::thread::spawn(move || {
            while !stop_t.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // On macOS/BSD an accepted socket INHERITS O_NONBLOCK
                        // from its listener, so the first read returns
                        // WouldBlock and the request is never seen. The
                        // listener is non-blocking so the accept loop can
                        // notice the stop flag; the connection must not be.
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        // Served one at a time, matching the console's
                        // one-request-per-connection posture: this brokers for
                        // a single connector subprocess, not for a fleet.
                        let _ = serve_one(
                            stream,
                            &policy,
                            &credentials,
                            &cache_t,
                            &refusals_t,
                            &calls_t,
                            &declared_t,
                            &owners_t,
                            &principal_t,
                            &blobs_t,
                            &artifact_store_t,
                            &blob_reads_t,
                            &grants,
                            &by_token,
                            refusal_code,
                        );
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Broker {
            url: format!("http://127.0.0.1:{port}"),
            stop,
            handle: Some(handle),
            refusals,
            calls,
            declared,
            credential_owners,
            run_principal,
            blobs,
            artifact_store,
            blob_reads,
            tokens,
            default_token,
        })
    }

    /// Serve `POST /blob` from this memory's CAS store (#106).
    ///
    /// The path, not a handle: [`areev_store::read_blob_offline`] never opens
    /// the memory, so serving a blob cannot contend with the run holding it.
    /// On the embedded backend it reads the `.blobs` sidecar beside the file,
    /// avoiding the driver's exclusive write lock; on postgres it opens its
    /// own short-lived connection and reads the in-schema `blobs` table, which
    /// takes no lock at all. That is the same property that lets a
    /// `--tool-cmd` subprocess run `areev blob get` mid-run.
    ///
    /// Until a host calls this, blob reads are refused whatever a module
    /// declared — declaring is not granting here either.
    pub fn serve_blobs(&self, db_path: &str) {
        if let Ok(mut b) = self.blobs.lock() {
            *b = Some(db_path.to_string());
        }
    }

    /// Bind the run's open, writable memory to the broker. No separate open
    /// or sidecar plaintext write: `put_blob` honors backend and encryption.
    pub fn bind_artifact_store(&self, store: Arc<AreevFacade>) {
        if let Ok(mut slot) = self.artifact_store.lock() {
            *slot = Some(store);
        }
    }

    /// Every blob a caller read, including an artifact upload, for journaling.
    pub fn blob_reads(&self) -> Vec<BlobRead> {
        self.blob_reads.lock().map(|b| b.clone()).unwrap_or_default()
    }

    /// Bind `name` to the run principal that owns it (#101 follow-through).
    ///
    /// An owned credential is attached only for a run whose bound principal
    /// equals the owner — a run executing as anyone else, or a path that
    /// never bound a principal at all, is refused. This is what stops a
    /// process loaded with several principals' credentials from letting a
    /// run started on behalf of one spend another's: the grant says which
    /// TOOLS may ask, and this says which RUNS may be answered.
    pub fn bind_credential_owner(&self, name: &str, principal: &str) {
        if let Ok(mut owners) = self.credential_owners.lock() {
            owners.insert(name.to_string(), principal.to_string());
        }
    }

    /// Record the principal the current run executes as.
    ///
    /// Called by the driver at drive entry — every run, including resume and
    /// fork, passes through there — so the binding cannot be forgotten by a
    /// caller. One broker serves one evaluation pass at a time (its own
    /// documented lifetime), which is what makes a single slot sound; a host
    /// that interleaved principals through one broker would need one broker
    /// per principal, and gets fail-closed behaviour rather than
    /// mis-attribution if it forgets.
    pub fn bind_run_principal(&self, principal: &str) {
        if let Ok(mut p) = self.run_principal.lock() {
            *p = Some(principal.to_string());
        }
    }

    /// Register what `caller`'s Definition declared, so the broker can enforce
    /// `declared ∩ host-granted` on every call (#101).
    ///
    /// Idempotent per caller in the sense that re-registering the same
    /// declaration is harmless — but it deliberately does NOT reset the call
    /// counter, so a tool dispatched repeatedly cannot buy itself a fresh
    /// budget by being re-declared.
    ///
    /// A caller that never declares is unaffected: `--tool-cmd` tools and
    /// connectors keep exactly the host-grant-only behaviour they have today.
    pub fn declare(
        &self,
        caller: &str,
        declaration: areev_core::types::capability::Declaration,
        limits: CapabilityLimits,
    ) {
        if let Ok(mut map) = self.declared.lock() {
            let entry = map.entry(caller.to_string()).or_default();
            entry.declaration = declaration;
            entry.limits = limits;
        }
    }

    /// The capability token `caller` presents, if it has a grant.
    pub fn token_for(&self, caller: &str) -> Option<&str> {
        self.tokens
            .get(caller)
            .or(self.default_token.as_ref())
            .map(String::as_str)
    }

    /// Every call that actually went out, in order (#101).
    ///
    /// Read rather than drained, like [`Broker::refusals`]: the driver
    /// journals them and the CLI prints a summary. NOT deduplicated — a
    /// refusal is a policy fact and forty attempts are one of them, but a
    /// successful call is an *effect*, and forty of those are forty things
    /// that happened.
    pub fn calls(&self) -> Vec<EgressCall> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// What to put in the connector's `AREEV_EGRESS_URL`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every DISTINCT refusal so far.
    ///
    /// Deduplicated at the point of recording, on `(caller, destination,
    /// reason)`. A tool retrying against one blocked host is one audit fact,
    /// not forty — which is what bounds this list by the plan's shape rather
    /// than by how hard something retries. The count of attempts is not kept
    /// here; the operator-facing log line is per attempt.
    ///
    /// Read rather than drained, so several consumers can each see the whole
    /// set: the driver journals them, the CLI prints them when the run ends,
    /// and the trigger evaluator turns them into `TRG-E009`.
    pub fn refusals(&self) -> Vec<EgressRefusal> {
        self.refusals.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        // Signal and detach — deliberately NOT joined. The accept loop can be
        // inside `serve_one` waiting on an upstream that has up to 60s of
        // timeout left, and joining there would block the evaluator on a call
        // whose result nobody wants any more. The thread owns its listener and
        // exits at the next loop check, so the port is released promptly
        // without anyone waiting for it.
        self.stop.store(true, Ordering::Relaxed);
        drop(self.handle.take());
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct EgressRequest {
    url: String,
    method: String,
    /// Which credential to attach, by name. The connector chooses *which*, never
    /// *what* — it cannot name a value it was not given.
    credential: Option<String>,
    body: Option<String>,
    /// Explicit byte mode. Text callers omit this and keep the original contract.
    response_mode: Option<String>,
    /// CAS address of request bytes, mutually exclusive with `body`.
    body_ref: Option<String>,
    /// Required with `body_ref`; treated as a declared request header.
    content_type: Option<String>,
    /// Non-credential request headers the caller wants set (#105).
    ///
    /// The enterprise APIs capability tools are pitched at need one:
    /// `X-Goog-User-Project` on every Google call made with user credentials,
    /// `anthropic-version`, `x-ms-version`, a tenant id. None of them is a
    /// secret — the caller supplies the value, so it is guest-visible by
    /// construction, which is why these may be journaled verbatim while a
    /// credential may only ever be journaled by name.
    ///
    /// A `BTreeMap` deliberately: one value per name, ordered, so the journal
    /// is deterministic and a caller cannot smuggle a second `X-Foo` past a
    /// check that looked at the first.
    headers: BTreeMap<String, String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct BlobRequest {
    /// A `cas://sha256:<64 hex>` content address. The ONLY way in: there is no
    /// enumeration and no name lookup, so a module reaches bytes it was handed
    /// a reference to and cannot go looking for others.
    uri: String,
}

/// `POST /blob` — hand a declared capability tool the bytes at one content
/// address (#106).
///
/// ## Why this is the broker's job and not the sandbox's
///
/// The obvious design is for the sandbox subprocess to read the `.blobs`
/// sidecar itself: the read is lock-free, so it looks free. It is not. The
/// subprocess runs under `EnvPolicy::ClearExcept` and is handed no memory
/// path; `areev-sandbox` deliberately carries five dependencies and cannot
/// take `areev-store`, so it would need its own `cas://` parser and its own
/// SHA-256; the `.blobs` sidecar does not exist on the Postgres backend, where
/// blobs live in-schema; and — decisively — a read performed inside the
/// subprocess has **no way back to the driver to be journaled**, because
/// stdout is contractually the guest's own result and the fuel line on stderr
/// is prose for a human.
///
/// Routing it here answers all four at once. The broker already runs in the
/// engine's process, already authenticates per-caller tokens, already has a
/// place to record what happened, and is already the thing that turns "the
/// module has no socket" into "every byte is mediated and recorded". This
/// makes the second half true of stored bytes as well: **the guest gets
/// neither a socket nor a file descriptor.**
#[allow(clippy::too_many_arguments)]
fn serve_blob(
    stream: &mut TcpStream,
    body: &[u8],
    caller: &str,
    declared: &Arc<std::sync::Mutex<BTreeMap<String, Declared>>>,
    blobs: &Arc<std::sync::Mutex<Option<String>>>,
    blob_reads: &Arc<std::sync::Mutex<Vec<BlobRead>>>,
    refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
    refusal_code: &'static str,
) -> std::io::Result<()> {
    let req: BlobRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return respond(
                stream,
                400,
                &serde_json::json!({ "error": format!("bad blob request: {e}") }).to_string(),
            )
        }
    };

    // Declared ∩ host-granted, the same two-key rule egress lives under: the
    // module must have declared `{"blob": {"read": true}}`, AND the host must
    // have wired a memory to serve from. A caller with no declaration at all
    // — every `--tool-cmd` tool, every connector — is refused here rather than
    // waved through, because unlike egress there is no host-side grant that
    // could have admitted it: subprocess tools already read blobs with
    // `areev blob get`, so this door exists only for the sandboxed ones.
    let permitted = declared
        .lock()
        .map(|d| d.get(caller).is_some_and(|e| e.declaration.declares_blob_read()))
        .unwrap_or(false);
    if !permitted {
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.to_string(),
                destination: req.uri.clone(),
                reason: "read a CAS blob without declaring the 'blob' capability".into(),
            },
        );
        return respond(
            stream,
            403,
            &serde_json::json!({
                "error": format!(
                    "caller '{caller}' asked to read a blob without declaring \
                     {{\"blob\": {{\"read\": true}}}}"
                ),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    let Some(db_path) = blobs.lock().ok().and_then(|b| b.clone()) else {
        return respond(
            stream,
            503,
            &serde_json::json!({
                "error": "this host wired no memory for blob reads"
            })
            .to_string(),
        );
    };

    match areev_store::read_blob_offline(&db_path, &req.uri) {
        Ok(Some(bytes)) => {
            note_blob_read(
                blob_reads,
                BlobRead {
                    caller: caller.to_string(),
                    uri: req.uri.clone(),
                    bytes: bytes.len(),
                },
            );
            respond_bytes(stream, &bytes)
        }
        // A sealed blob needs the memory's derived key, which lives behind an
        // open handle this lock-free path deliberately does not take. Said
        // plainly rather than reported as missing: "encrypted" and "not there"
        // are different problems with different fixes.
        Ok(None) => respond(
            stream,
            409,
            &serde_json::json!({
                "error": format!(
                    "blob {} is encrypted at rest; a sandboxed tool cannot open it",
                    req.uri
                )
            })
            .to_string(),
        ),
        Err(e) => respond(
            stream,
            404,
            &serde_json::json!({ "error": e.to_string() }).to_string(),
        ),
    }
}

/// Ceiling on how many effects one broker keeps in memory for journaling.
/// Shared by calls and blob reads: both are effects, both are drained at the
/// superstep boundary, and neither should let a runaway module grow the
/// driver's heap without bound.
const MAX_RECORDED_CALLS: usize = 4096;

/// Record one blob read. Unlike a refusal these are NOT deduped: reading the
/// same attachment twice is two reads, exactly as two successful calls are two
/// calls.
fn note_blob_read(reads: &Arc<std::sync::Mutex<Vec<BlobRead>>>, r: BlobRead) {
    if let Ok(mut list) = reads.lock() {
        if list.len() < MAX_RECORDED_CALLS {
            list.push(r);
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Record a refusal once. Bounded by distinct refusals, not by attempts.
fn note_refusal(refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>, r: EgressRefusal) {
    if let Ok(mut list) = refusals.lock() {
        if !list.contains(&r) {
            list.push(r);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_one(
    mut stream: TcpStream,
    policy: &EgressPolicy,
    credentials: &BTreeMap<String, CredentialSource>,
    cache: &CredentialCache,
    refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
    calls: &Arc<std::sync::Mutex<Vec<EgressCall>>>,
    declared: &Arc<std::sync::Mutex<BTreeMap<String, Declared>>>,
    credential_owners: &Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    run_principal: &Arc<std::sync::Mutex<Option<String>>>,
    blobs: &Arc<std::sync::Mutex<Option<String>>>,
    artifact_store: &Arc<std::sync::Mutex<Option<Arc<AreevFacade>>>>,
    blob_reads: &Arc<std::sync::Mutex<Vec<BlobRead>>>,
    grants: &EgressGrants,
    by_token: &BTreeMap<String, String>,
    refusal_code: &'static str,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    let mut reader = BufReader::new(stream.try_clone()?);

    // Request line, then headers, then a Content-Length body.
    let mut line = String::new();
    reader.read_line(&mut line)?;
    // Two doors, one port and one token: `/` brokers a network call, `/blob`
    // reads stored bytes (#106). A path rather than a discriminant inside the
    // JSON, so the two request shapes stay separate types and a blob read can
    // be refused before anything parses an egress request out of it.
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let want_blob = path.starts_with("/blob");
    let mut content_length = 0usize;
    let mut presented: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        // Read off the ORIGINAL line: lowercasing the header name must not
        // lowercase the token's value.
        if lower.starts_with("x-areev-egress-token:") {
            presented = h.split_once(':').map(|(_, v)| v.trim().to_string());
        }
    }

    // Loopback is not an authorization: without this, any process on the box
    // could post here and spend the credentials the broker holds. It is also
    // what lets one port serve N pool workers and still tell them apart.
    let caller = match presented.and_then(|t| by_token.get(&t).cloned()) {
        Some(c) => c,
        None => {
            // The caller may still be mid-write on a body we are about to
            // refuse to read for its own sake; drain it so closing the
            // connection sends a clean FIN, not an RST that could clobber
            // their view of this very response.
            drain_body(&mut reader, content_length);
            return respond(
                &mut stream,
                401,
                &serde_json::json!({
                    "error": "missing or unknown X-Areev-Egress-Token",
                    "code": refusal_code
                })
                .to_string(),
            )
        }
    };
    let Some(grant) = grants.for_caller(&caller) else {
        drain_body(&mut reader, content_length);
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!("caller '{caller}' has no egress grant"),
                "code": refusal_code
            })
            .to_string(),
        );
    };
    if content_length > MAX_BODY {
        // Deliberately NOT drained: the whole point of this refusal is that
        // the caller claims a body large enough that reading it is the
        // resource-exhaustion risk. An abrupt reset here is the acceptable
        // side of that trade, unlike the two refusals above where the body is
        // always small and legitimate.
        return respond(&mut stream, 413, r#"{"error":"body too large"}"#);
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    if want_blob {
        return serve_blob(
            &mut stream,
            &body,
            &caller,
            declared,
            blobs,
            blob_reads,
            refusals,
            refusal_code,
        );
    }

    let req: EgressRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return respond(
                &mut stream,
                400,
                &serde_json::json!({ "error": format!("bad egress request: {e}") }).to_string(),
            )
        }
    };

    // Guest headers, validated before anything else looks at them (#105).
    //
    // Two refusals with different characters. A malformed name or value is a
    // CLIENT error — the same shape as unparseable JSON — and a value carrying
    // CR/LF is header injection, which must die here rather than at the socket
    // where it would split one request into two.
    //
    // A broker-owned name is a policy refusal, and deliberately a FREE one:
    // the "probing is not free" rule that spends a call before the declaration
    // check exists because those answers differ per caller and so leak the
    // policy. "May I write the Authorization header?" has exactly one answer,
    // no, for every caller that ever asks — so answering it without charge
    // reveals nothing, and refusing early keeps the credential channel's
    // guarantee independent of budgets, declarations, and grants.
    for (name, value) in &req.headers {
        if !areev_core::types::capability::is_valid_header_name(name)
            || !areev_core::types::capability::is_valid_header_value(value)
        {
            return respond(
                &mut stream,
                400,
                &serde_json::json!({
                    "error": format!("header '{name}' is not a valid HTTP header name/value")
                })
                .to_string(),
            );
        }
        if areev_core::types::capability::is_broker_owned_header(name) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: req.url.clone(),
                    reason: format!("tried to set the broker-owned header '{name}'"),
                },
            );
            return respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!(
                        "caller '{caller}' tried to set header '{name}', which the broker owns — \
                         name a credential instead; the broker attaches it and the caller never \
                         holds one"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            );
        }
    }
    if req.response_mode.as_deref().is_some_and(|m| m != "artifact") {
        return respond(&mut stream, 400, r#"{"error":"unsupported response_mode (expected artifact)"}"#);
    }
    if req.body.is_some() && req.body_ref.is_some() {
        return respond(&mut stream, 400, r#"{"error":"body and body_ref are mutually exclusive"}"#);
    }
    if req.content_type.is_some() != req.body_ref.is_some() {
        return respond(&mut stream, 400, r#"{"error":"content_type is required exactly when body_ref is set"}"#);
    }
    let mut guest_headers: Vec<(String, String)> =
        req.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(mime) = &req.content_type {
        if !areev_core::types::capability::is_valid_header_value(mime) || mime.is_empty()
            || req.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type"))
        {
            return respond(&mut stream, 400, r#"{"error":"invalid or duplicate content_type"}"#);
        }
        guest_headers.push(("Content-Type".into(), mime.clone()));
    }
    let guest_header_names: Vec<String> = guest_headers.iter().map(|(k, _)| k.clone()).collect();
    let sent_header_names: BTreeMap<String, String> = guest_headers.iter().cloned().collect();

    if let Err(e) = policy.permits(&req.url) {
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.clone(),
                destination: req.url.clone(),
                reason: "destination outside the declared allowlist".into(),
            },
        );
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!("caller '{caller}' {e}"),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    let requested_method = if req.method.trim().is_empty() { "GET" } else { req.method.trim() };
    let requested_method = requested_method.to_ascii_uppercase();
    // An older broker ignores unknown JSON fields, including body_ref. It
    // MUST NOT turn a requested artifact upload into an empty POST. The
    // marker is deliberately not a legacy HTTP method: old peers refuse it
    // before dispatch, while this broker maps it to the real, granted verb.
    let method = match (req.body_ref.is_some(), requested_method.as_str()) {
        (true, "POST_ARTIFACT") => "POST",
        (true, "PUT_ARTIFACT") => "PUT",
        (true, "PATCH_ARTIFACT") => "PATCH",
        (false, "POST_ARTIFACT" | "PUT_ARTIFACT" | "PATCH_ARTIFACT") => {
            return respond(&mut stream, 400, r#"{"error":"artifact method requires body_ref"}"#)
        }
        (true, _) => return respond(&mut stream, 400, r#"{"error":"body_ref requires POST_ARTIFACT, PUT_ARTIFACT, or PATCH_ARTIFACT"}"#),
        (false, _) => requested_method.as_str(),
    }.to_string();

    // The DECLARED half of the intersection (#101), checked alongside the host
    // grant and never instead of it: a declaration can only narrow. A caller
    // with no declaration — every `--tool-cmd` tool and every connector — skips
    // this entirely and is governed by the host grant alone, as before.
    //
    // The call budget is spent here, before dispatch, so a refused call still
    // costs one: otherwise a module could probe the policy for free, and the
    // ceiling would bound successes rather than attempts. The declaration is
    // CLONED out of the registry so `dispatch` can re-apply it to every
    // redirect hop without holding the lock across network I/O — a hop is a
    // destination the caller never named, and it does not get to be the one
    // place the declared half goes unchecked.
    let capability: Option<(CapabilityLimits, Declaration)> = {
        let mut guard = declared.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&caller) {
            None => None,
            Some(d) => {
                if d.calls_made >= d.limits.max_calls {
                    let max = d.limits.max_calls;
                    drop(guard);
                    note_refusal(
                        refusals,
                        EgressRefusal {
                            caller: caller.clone(),
                            destination: req.url.clone(),
                            reason: format!("exceeded its ceiling of {max} brokered calls"),
                        },
                    );
                    return respond(
                        &mut stream,
                        403,
                        &serde_json::json!({
                            "error": format!(
                                "caller '{caller}' has made its {max} permitted brokered calls"
                            ),
                            "code": refusal_code
                        })
                        .to_string(),
                    );
                }
                d.calls_made += 1;
                if let Err(denied) = d.declaration.permits(
                    &req.url,
                    &method,
                    req.credential.as_deref(),
                    &guest_header_names,
                ) {
                    drop(guard);
                    note_refusal(
                        refusals,
                        EgressRefusal {
                            caller: caller.clone(),
                            destination: req.url.clone(),
                            reason: format!("undeclared capability: {denied}"),
                        },
                    );
                    return respond(
                        &mut stream,
                        403,
                        &serde_json::json!({
                            "error": format!("caller '{caller}' {denied}"),
                            "code": refusal_code
                        })
                        .to_string(),
                    );
                }
                Some((d.limits, d.declaration.clone()))
            }
        }
    };

    if (req.body_ref.is_some() || req.response_mode.as_deref() == Some("artifact"))
        && artifact_store.lock().ok().and_then(|s| s.clone()).is_none()
    {
        return respond(&mut stream, 503, r#"{"error":"artifact mode requires a run-bound writable memory"}"#);
    }
    if req.body_ref.is_some() && !capability.as_ref().is_some_and(|(_, d)| d.declares_blob_read()) {
        return respond(&mut stream, 403, &serde_json::json!({
            "error": "body_ref requires a declared blob read capability", "code": refusal_code
        }).to_string());
    }

    // The EFFECTIVE byte ceilings (#339), computed once so the read cap and
    // every message that names a limit report the same number. An artifact
    // is bounded by the caller's declaration — 1 MiB when it declared none —
    // up to the 32 MiB hard maximum; text keeps its original contract
    // (declared ceiling for a capability caller, uncapped for a subprocess
    // tool or connector).
    let artifact_mode = req.response_mode.as_deref() == Some("artifact");
    let response_cap: Option<usize> = match (&capability, artifact_mode) {
        (Some((l, _)), _) => Some(l.max_response_bytes),
        (None, true) => Some(DEFAULT_TRANSFER),
        (None, false) => None,
    };
    let request_cap = capability.as_ref().map_or(DEFAULT_TRANSFER, |(l, _)| l.max_request_bytes);
    // Re-checked here, not only at registration: `CapabilityLimits` is public,
    // and a host that built one by hand must get the refusal the declaration
    // path gives rather than a quiet clamp. Before any upstream I/O.
    let out_of_range = |key: &str, n: usize| -> Option<String> {
        (n == 0 || n > MAX_TRANSFER).then(|| {
            areev_run_core::RunError::TransferLimitInvalid {
                node: caller.clone(),
                detail: format!(
                    "{key} is {n}, outside 1..={MAX_TRANSFER} — refused rather than clamped"
                ),
            }
            .to_string()
        })
    };
    let invalid = if artifact_mode {
        out_of_range("max_response_bytes", response_cap.unwrap_or(DEFAULT_TRANSFER))
    } else {
        None
    }
    .or_else(|| req.body_ref.as_ref().and_then(|_| out_of_range("max_request_bytes", request_cap)));
    if let Some(detail) = invalid {
        return respond(
            &mut stream,
            400,
            &serde_json::json!({ "error": detail, "code": "RUN-E028" }).to_string(),
        );
    }

    // For a capability caller, an unrestricted host policy does not extend to
    // private address space (#101). A memory that syncs in can declare any
    // hosts it likes, so the declaration alone must never be what authorizes
    // a request to the loopback console, a cloud metadata service,
    // or a LAN neighbour — reaching those takes an explicit `--allow-host`
    // entry, an operator's auditable act, exactly as executing the blob at
    // all takes the executor pin. Subprocess tools and connectors are
    // untouched: their reach was always pure host config.
    if capability.is_some() && policy.is_unrestricted() && crate::egress::is_private_destination(&req.url)
    {
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.clone(),
                destination: req.url.clone(),
                reason: "private or loopback destination without an explicit --allow-host entry"
                    .into(),
            },
        );
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!(
                    "caller '{caller}' tried to reach a private or loopback destination — a \
                     capability declaration alone cannot authorize one; the host must name it \
                     in --allow-host"
                ),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    // Connectors read; tools write. A grant that names no method may only
    // read, so the write verb is always something someone decided to allow.
    if !grant.permits_method(&method) {
        let denied = EgressDenied::Method { method: method.clone() };
        note_refusal(
            refusals,
            EgressRefusal {
                caller: caller.clone(),
                destination: req.url.clone(),
                reason: format!("method {method} is not permitted for this caller"),
            },
        );
        return respond(
            &mut stream,
            403,
            &serde_json::json!({
                "error": format!("caller '{caller}' {denied}"),
                "code": refusal_code
            })
            .to_string(),
        );
    }

    // Resolve the credential to a header pair before dispatch. The connector
    // chooses WHICH credential by name; it can never name a value it was not
    // given, and no value ever crosses back to it.
    let mut headers: Vec<(String, String)> = Vec::new();
    // Kept so the 401 path below knows whether re-minting is even possible: a
    // static credential that draws a 401 is simply wrong, and retrying it
    // would spend a second call to be told so again.
    let mut credential_source: Option<&CredentialSource> = None;
    if let Some(name) = &req.credential {
        // Owned credentials bind to a RUN principal, not just to a tool. The
        // grant says which tools may ask; this says which runs may be
        // answered — a process loaded with several principals' credentials
        // must not let a run started on behalf of one spend another's. Fails
        // closed when no principal was bound at all: a path that never binds
        // (the trigger evaluator's connector pass, a bare embedding) gets a
        // refusal, never a quiet exception.
        // Recover a poisoned guard rather than fail OPEN on it: `.lock().ok()`
        // would yield `None`, skip the owner check entirely, and attach the
        // credential with no principal binding — disabling exactly the #101
        // isolation this block enforces. The `declared` map above already fails
        // closed this way (`into_inner`); the security-critical owner map must
        // too.
        let owner = {
            let owners = credential_owners.lock().unwrap_or_else(|e| e.into_inner());
            owners.get(name.as_str()).cloned()
        };
        if let Some(owner) = owner {
            let bound = run_principal.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if bound.as_deref() != Some(owner.as_str()) {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.clone(),
                        destination: req.url.clone(),
                        reason: format!(
                            "credential '{name}' is bound to principal '{owner}', and this run \
                             executes as {}",
                            bound.as_deref().unwrap_or("no bound principal")
                        ),
                    },
                );
                // The response names neither principal: the caller is a tool,
                // and whose credential this is happens to be none of its
                // business. The journaled refusal above carries both for the
                // operator.
                return respond(
                    &mut stream,
                    403,
                    &serde_json::json!({
                        "error": format!(
                            "caller '{caller}' may not use credential '{name}' — it is bound to \
                             a different run principal"
                        ),
                        "code": refusal_code
                    })
                    .to_string(),
                );
            }
        }
        // Scoped per caller AND per destination: naming a credential is not
        // the same as being allowed to use it, and being allowed to use it is
        // not the same as being allowed to send it *there* (#112). The host
        // half of the pairing, checked beside the declared half above — the
        // declaration travels with the tool, so it cannot be the only thing
        // deciding where a secret may go.
        if let Err(denied) = grant.permits_credential(name, &req.url) {
            let (reason, detail) = match denied {
                CredentialDenied::NotGranted => (
                    format!("credential '{name}' is not granted to this caller"),
                    format!("caller '{caller}' may not use credential '{name}'"),
                ),
                CredentialDenied::WrongHost => (
                    format!(
                        "credential '{name}' is granted to this caller only for other hosts"
                    ),
                    format!(
                        "caller '{caller}' may not send credential '{name}' to this host — the \
                         grant pairs it with a different one"
                    ),
                ),
            };
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: req.url.clone(),
                    reason,
                },
            );
            return respond(
                &mut stream,
                403,
                &serde_json::json!({ "error": detail, "code": refusal_code }).to_string(),
            );
        }
        let Some(source) = credentials.get(name) else {
            return respond(
                &mut stream,
                400,
                &serde_json::json!({
                    "error": format!("no credential named '{name}' is configured for this run")
                })
                .to_string(),
            );
        };
        // Resolved HERE, at call time, which is what lets a source mint a
        // fresh short-lived token per TTL window rather than per process
        // (#113). A static source is a clone and cannot fail.
        match resolve_credential(name, source, cache) {
            Ok(Credential::Bearer(v)) => {
                headers.push(("Authorization".into(), format!("Bearer {v}")))
            }
            Ok(Credential::Header { name, value }) => headers.push((name.clone(), value.clone())),
            // Fail CLOSED: no credential, no call.
            //
            // The detail goes to the OPERATOR (the journaled refusal), and the
            // guest is told only that the credential could not be resolved —
            // the same split the principal-binding refusal makes above, for the
            // same reason. A resolver diagnostic names infrastructure the guest
            // has no business learning: a vault's address, its mount, the
            // secret's path and field. The caller is code that may have arrived
            // in a synced memory; that its credential is unavailable is all it
            // needs, and all it gets. Neither side ever sees what the resolver
            // printed.
            Err(detail) => {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.clone(),
                        destination: req.url.clone(),
                        reason: detail,
                    },
                );
                return respond(
                    &mut stream,
                    403,
                    &serde_json::json!({
                        "error": format!(
                            "caller '{caller}' could not be given credential '{name}' — its \
                             source did not resolve; the run's audit trail records why"
                        ),
                        "code": refusal_code
                    })
                    .to_string(),
                );
            }
        }
        credential_source = Some(source);
    }

    // The static list of broker-owned names is not the whole credential
    // channel: `Credential::Header` lets an operator carry a secret in ANY
    // header — `X-Api-Key`, `apikey`, whatever the upstream wants — and that
    // name is known only here, after resolution, because it is per-credential
    // host configuration rather than a constant. A guest header colliding with
    // it would be a guest writing a value into the exact slot the broker fills
    // with a secret. Refused for the same reason and with the same words
    // (#105).
    for (guest_name, _) in &guest_headers {
        let g = guest_name.trim().to_ascii_lowercase();
        if headers.iter().any(|(k, _)| k.trim().to_ascii_lowercase() == g) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: req.url.clone(),
                    reason: format!("tried to set the broker-owned header '{guest_name}'"),
                },
            );
            return respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!(
                        "caller '{caller}' tried to set header '{guest_name}', which carries a \
                         configured credential on this host — name the credential instead"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            );
        }
    }

    if !matches!(method.as_str(), "GET" | "HEAD" | "DELETE" | "POST" | "PUT" | "PATCH") {
        return respond(
            &mut stream,
            400,
            &serde_json::json!({ "error": format!("unsupported method '{method}'") }).to_string(),
        );
    }

    // A `body_ref` upload is sized against its EFFECTIVE ceiling here, before
    // `dispatch` opens a connection: an overrun is refused before the upstream
    // sees a byte, and the refusal names the limit that applied (#339).
    let request_bytes = if let Some(uri) = &req.body_ref {
        let store = artifact_store.lock().unwrap().clone().unwrap();
        match store.with_store(|m| m.get_blob(uri)) {
            Ok(bytes) if bytes.len() <= request_cap => {
                note_blob_read(blob_reads, BlobRead { caller: caller.clone(), uri: uri.clone(), bytes: bytes.len() });
                Some(bytes)
            }
            Ok(bytes) => {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.clone(),
                        destination: req.url.clone(),
                        reason: format!("artifact upload exceeds its {request_cap}-byte ceiling"),
                    },
                );
                return respond(
                    &mut stream,
                    413,
                    &serde_json::json!({
                        "error": format!(
                            "caller '{caller}' asked to upload a {}-byte artifact, larger than \
                             its {request_cap}-byte max_request_bytes ceiling — refused before \
                             any byte was sent upstream",
                            bytes.len()
                        ),
                        "code": refusal_code
                    })
                    .to_string(),
                );
            }
            Err(e) => return respond(&mut stream, 400, &serde_json::json!({"error": e.to_string()}).to_string()),
        }
    } else { req.body.as_ref().map(|s| s.as_bytes().to_vec()) };

    let mut outcome = dispatch(
        &req.url,
        &method,
        request_bytes.as_deref(),
        &headers,
        &guest_headers,
        policy,
        grant,
        capability.as_ref().map(|(_, d)| d),
        response_cap,
        artifact_mode,
        &caller,
        refusals,
    );

    // A 401 on a MINTED credential is the expiry case this seam exists for
    // (#113): the cached token lapsed upstream before its TTL lapsed here.
    // Invalidate it always — the next call then mints a fresh one — and
    // re-issue this call only when doing so is safe.
    //
    // "Safe" is decided by the METHOD, which the broker knows because it is
    // the party that dispatched it. GET and HEAD are idempotent, so replaying
    // one costs nothing and turns "the token expired mid-run" from an incident
    // into a non-event. Anything that may have changed state upstream is NOT
    // replayed: a POST that 401'd may still have been applied, and the broker
    // is not entitled to guess. Its caller sees the 401 and decides, with a
    // freshly-minted credential waiting for the retry.
    //
    // Bounded to one re-issue by construction — there is no loop here.
    if let (Some(name), Some(source)) = (&req.credential, credential_source) {
        // `credential_sent` is load-bearing here, not decoration. A chain that
        // left its start origin drops the credential (`left_origin`), so a 401
        // from the redirect TARGET says nothing about our token — it says that
        // host wanted its own auth. Without this guard an allowed third-party
        // host reached only by redirect would decide when our credential cache
        // is flushed, and since the flush repopulates and the next call repeats
        // it, the TTL cache would collapse into one resolver subprocess per
        // call: exactly the per-call fork the cache exists to prevent.
        let unauthorized =
            matches!(&outcome, Dispatched::Answered { status: 401, credential_sent: true, .. });
        if unauthorized && source.is_dynamic() {
            invalidate_credential(name, cache);
            if matches!(method.as_str(), "GET" | "HEAD") {
                if let Ok(fresh) = resolve_credential(name, source, cache) {
                    // The discarded attempt is still an EFFECT: a request went
                    // out, carrying a credential, and an upstream answered it.
                    // Journaling only the retry would let the audit trail say
                    // one call happened where two did — and `calls()` is
                    // deliberately not deduplicated precisely because forty
                    // successful calls are forty things that happened. The
                    // caller never sees this response; the record does.
                    if let Dispatched::Answered {
                        status, body, final_url, redirects, credential_sent, ..
                    } = &outcome
                    {
                        note_call(
                            calls,
                            EgressCall {
                                caller: caller.clone(),
                                method: method.clone(),
                                url: final_url.clone(),
                                status: *status,
                                redirects: *redirects,
                                request_digest: request_bytes.as_deref().map(digest_bytes),
                                // Scrubbed before digesting, exactly as the
                                // returned body is: an endpoint that echoed
                                // the expired token must not put it inside a
                                // digest either.
                                response_digest: if req.response_mode.as_deref() == Some("artifact") {
                                    digest_bytes(body)
                                } else {
                                    digest_bytes(scrub_reflected(
                                        String::from_utf8(body.clone()).expect("text decoded at dispatch"),
                                        &headers).as_bytes())
                                },
                                response_bytes: body.len(),
                                // The discarded 401 may reflect a token in
                                // its header; do not journal an untrusted MIME.
                                response_mime: None,
                                response_ref: None,
                                credential: if *credential_sent {
                                    req.credential.clone()
                                } else {
                                    None
                                },
                                headers: if *credential_sent {
                                    sent_header_names.clone()
                                } else {
                                    BTreeMap::new()
                                },
                            },
                        );
                    }
                    let refreshed: Vec<(String, String)> = match fresh {
                        Credential::Bearer(v) => {
                            vec![("Authorization".into(), format!("Bearer {v}"))]
                        }
                        Credential::Header { name, value } => vec![(name, value)],
                    };
                    outcome = dispatch(
                        &req.url,
                        &method,
                        request_bytes.as_deref(),
                        &refreshed,
                        &guest_headers,
                        policy,
                        grant,
                        capability.as_ref().map(|(_, d)| d),
                        response_cap,
                        artifact_mode,
                        &caller,
                        refusals,
                    );
                    // The reflection scrub below iterates `headers`, so the
                    // value it must scrub is the one that actually rode the
                    // retried request.
                    headers = refreshed;
                }
            }
        }
    }

    match outcome {
        Dispatched::Answered { status, body, mime, final_url, redirects, credential_sent } => {
            if headers.iter().any(|(_, value)| value.len() >= 8 && mime.contains(value)) {
                return respond(&mut stream, 502, r#"{"error":"upstream reflected a credential in media type"}"#);
            }
            // Credential reflection: an echo or a verbose error endpoint can
            // bounce the injected `Authorization` back in its BODY, and that
            // body goes to the guest and (as a digest) into the audit trail.
            // Response HEADERS never cross this boundary at all — the broker
            // answers with `{status, body}` and nothing else — so the body is
            // the only channel, and it is scrubbed rather than trusted.
            // Text is scrubbed; binary must remain byte-exact, so refuse a
            // reflected credential instead of silently changing the artifact.
            let body = if req.response_mode.as_deref() == Some("artifact") {
                if headers.iter().any(|(_, value)| value.len() >= 8 &&
                    (body.windows(value.len()).any(|w| w == value.as_bytes()) ||
                     value.strip_prefix("Bearer ").is_some_and(|bare| bare.len() >= 8 &&
                         body.windows(bare.len()).any(|w| w == bare.as_bytes())))) {
                    return respond(&mut stream, 502, r#"{"error":"upstream reflected a credential in binary response"}"#);
                }
                body
            } else {
                scrub_reflected(String::from_utf8(body).expect("text decoded at dispatch"), &headers).into_bytes()
            };
            let artifact_ref = if req.response_mode.as_deref() == Some("artifact") {
                let store = artifact_store.lock().unwrap().clone().unwrap();
                match store.with_store(|m| m.put_blob(&body)) {
                    Ok(uri) => Some(uri),
                    Err(e) => return respond(&mut stream, 502, &serde_json::json!({"error": format!("artifact storage failed: {e}")}).to_string()),
                }
            } else { None };
            note_call(
                calls,
                EgressCall {
                    caller: caller.clone(),
                    method: method.clone(),
                    url: final_url,
                    status,
                    redirects,
                    request_digest: request_bytes.as_deref().map(digest_bytes),
                    response_digest: digest_bytes(&body),
                    response_bytes: body.len(),
                    response_mime: req.response_mode.as_deref().map(|_| mime.clone()),
                    response_ref: artifact_ref.clone(),
                    // Only if the credential actually rode the FINAL request: a
                    // cross-origin redirect drops it (see `dispatch`), and an
                    // immutable audit grain claiming the secret reached a
                    // destination it never touched is a false record a DSAR or
                    // reviewer would read as fact.
                    credential: if credential_sent { req.credential.clone() } else { None },
                    // Recorded on the same "what actually rode the final
                    // request" rule as the credential, and for the same
                    // reason: guest headers travel exactly as far as the
                    // credential does (see `dispatch`), so a chain that left
                    // its origin sent neither.
                    headers: if credential_sent {
                        sent_header_names.clone()
                    } else {
                        BTreeMap::new()
                    },
                },
            );
            let answer = if req.response_mode.as_deref() == Some("artifact") {
                serde_json::json!({ "status": status,
                    "ref": artifact_ref,
                    "sha256": digest_bytes(&body), "bytes": body.len(), "mime": mime })
            } else {
                serde_json::json!({ "status": status,
                    "body": String::from_utf8(body).expect("text decoded at dispatch") })
            };
            respond(&mut stream, 200, &answer.to_string())
        }
        Dispatched::Refused { detail } => respond(
            &mut stream,
            403,
            &serde_json::json!({ "error": detail, "code": refusal_code }).to_string(),
        ),
        // Overruns are typed errors, never truncation: a tool handed half a
        // response computes a wrong answer with nothing to show for it. The
        // read was abandoned at the cap, so the oversized body was never
        // buffered whole.
        //
        // The number named is the EFFECTIVE ceiling the read was abandoned
        // at — the same value `dispatch` enforced — never the declaration
        // alone (#339: a 25 MiB declaration once reported itself while a
        // silent 1 MiB clamp was what actually refused).
        Dispatched::TooLarge { final_url } => {
            let max = response_cap.unwrap_or(0);
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: final_url,
                    reason: format!("response exceeds its {max}-byte ceiling"),
                },
            );
            respond(
                &mut stream,
                403,
                &serde_json::json!({
                    "error": format!(
                        "caller '{caller}' received a response larger than its {max}-byte \
                         ceiling — refused rather than truncated"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            )
        }
        // #298: a body that is not UTF-8 text is an error with the reason
        // named, not a 200 with an empty body. The caller can then fall back
        // to its own executor for that endpoint; it could not even detect the
        // old failure.
        Dispatched::NotText { final_url, detail } => {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.clone(),
                    destination: final_url,
                    reason: format!("response body is not UTF-8 text: {detail}"),
                },
            );
            respond(
                &mut stream,
                502,
                &serde_json::json!({
                    "error": format!(
                        "upstream: response body is not UTF-8 text ({detail}) — the \
                         broker carries text bodies only, and refuses rather than \
                         handing back an empty one with the upstream's status"
                    ),
                    "code": refusal_code
                })
                .to_string(),
            )
        }
        Dispatched::Upstream(e) => respond(
            &mut stream,
            502,
            &serde_json::json!({ "error": format!("upstream: {e}") }).to_string(),
        ),
    }
}

/// Replace any injected credential value the upstream echoed back.
///
/// An echo or a verbose error endpoint can bounce the injected `Authorization`
/// back in its BODY, and that body goes to the guest and (as a digest) into
/// the audit trail. Response HEADERS never cross this boundary at all — the
/// broker answers with `{status, body}` and nothing else — so the body is the
/// only channel, and it is scrubbed rather than trusted.
///
/// A free function rather than inline, so that every path which digests or
/// returns a body scrubs it under the same rule. The 401-retry path journals a
/// second body, and two audit rows recorded under different rules would be a
/// quiet inconsistency in the one record meant to be authoritative.
fn scrub_reflected(mut body: String, headers: &[(String, String)]) -> String {
    for (_, value) in headers {
        if value.len() >= 8 && body.contains(value.as_str()) {
            body = body.replace(value.as_str(), "[redacted-credential]");
        }
        if let Some(bare) = value.strip_prefix("Bearer ") {
            if bare.len() >= 8 && body.contains(bare) {
                body = body.replace(bare, "[redacted-credential]");
            }
        }
    }
    body
}

/// `sha256:<hex>` over a body. The audit trail records what was sent and
/// received without recording a mailbox into an immutable, replicating grain.
fn digest_bytes(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(body);
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// Record one successful call. Bounded so a runaway loop cannot exhaust
/// memory here; the ceiling is well above `CapabilityLimits::max_calls`, which
/// is the real bound for a capability tool.
fn note_call(calls: &Arc<std::sync::Mutex<Vec<EgressCall>>>, c: EgressCall) {
    if let Ok(mut list) = calls.lock() {
        if list.len() < MAX_RECORDED_CALLS {
            list.push(c);
        }
    }
}

/// How many hops the broker follows before giving up. Matches ureq's own
/// default so the change in *who* follows a redirect does not also change how
/// many are tolerated.
const MAX_REDIRECT_HOPS: usize = 10;

/// The outcome of one brokered call, redirects included.
enum Dispatched {
    /// The upstream answered. Any status the caller is entitled to see,
    /// including a `3xx` this broker chose not to follow.
    ///
    /// `final_url` is where the response actually came from, which is not
    /// necessarily where the caller aimed — that distinction is the point of
    /// recording it.
    ///
    /// `credential_sent` is whether the credential actually rode the FINAL
    /// request: a cross-origin redirect drops it, so the audit must not claim a
    /// secret reached a destination it never did.
    ///
    /// It answers the same question for the caller's own headers (#105), which
    /// is not a coincidence to be maintained by hand: `perform` gates both on
    /// one boolean, so "the credential rode" and "the guest headers rode" are
    /// the same fact. If they ever stop being the same fact, this needs to
    /// become two flags — journaling a header that did not travel is the same
    /// false record as journaling a credential that did not.
    Answered { status: u16, body: Vec<u8>, mime: String, final_url: String, redirects: u32, credential_sent: bool },
    /// A hop was refused by policy; already recorded in `refusals`.
    Refused { detail: String },
    /// The final response exceeded the caller's `max_response_bytes` — the
    /// read was ABANDONED at the cap, not completed and measured. A typed
    /// outcome rather than an oversized `Answered`, because the alternative on
    /// the error path was an empty string masquerading as the upstream's
    /// answer: a silent truncation to nothing, which is the exact failure mode
    /// the cap exists to make loud.
    TooLarge { final_url: String },
    /// The upstream answered with a status the caller is entitled to see, but
    /// the body could not be decoded as UTF-8 text (#298).
    ///
    /// A typed outcome for exactly the reason `TooLarge` is one. Both error
    /// paths used to fall through to an empty string with the REAL status
    /// attached — so a brokered connector fetching a PDF received
    /// `{"status":200,"body":""}` and the audit grain recorded a 0-byte
    /// success. That is a silent wrong answer, and `docs/run.md` rules out
    /// its shape for overruns in the same words: an overrun is an error,
    /// never a truncation.
    NotText { final_url: String, detail: String },
    /// The transport failed.
    Upstream(String),
}

/// Why a response body could not be handed back (#298).
enum BodyErr {
    /// Over the caller's ceiling; the read was abandoned at the cap.
    TooLarge,
    /// Not decodable as UTF-8 text.
    NotText(String),
    /// An artifact body the transport cut short (#339).
    Transport(String),
}

/// Perform the call, following redirects **by hand** so the allowlist governs
/// every hop rather than only the first (#99).
///
/// ## Why the client is not allowed to do this for us
///
/// ureq follows up to ten redirects on its own. The broker checked
/// `policy.permits` once, against the URL the *caller* supplied, before
/// dispatch — so an allowed host answering `302 Location:
/// http://169.254.169.254/latest/meta-data/` sent the follow-up request to the
/// cloud metadata service and handed its body back to the tool. Host
/// allowlisting is this subsystem's core control, and a redirect walked
/// straight through it. Auto-follow is therefore off (`max_redirects(0)`) and
/// each hop is re-authorized here: **no byte is sent to, and no body is
/// returned from, a host the allowlist does not permit.**
///
/// ## And the mirror image, which was a silent breakage
///
/// ureq's `redirect_auth_headers` defaults to `Never`, so it dropped the
/// brokered `Authorization` on *every* redirect — including a same-origin one,
/// which Google and Microsoft APIs use routinely. The follow-up arrived
/// unauthenticated, 401'd, and nothing in the journal said why. Here the
/// credential is re-attached exactly when [`crate::egress::same_origin`] holds
/// and dropped otherwise, so legitimate redirects work and cross-origin ones
/// still never see the secret.
///
/// ## Method transitions
///
/// The same rules ureq applied, kept so the change is in authorization and not
/// in semantics: 303 and the historical 301/302 turn a non-GET into a GET;
/// 307/308 retain the method, and a retained method that carries a body is not
/// resent at all — the `3xx` goes back to the caller, which is what ureq did.
/// A changed method is re-checked against the grant, because a grant that
/// permits POST does not thereby permit the GET a 303 turns it into (and vice
/// versa: a read-only caller must not be walked into a write).
///
/// ## Capability tools get their declaration on every hop too (#101)
///
/// For a caller with a declaration, each hop is additionally checked against
/// it — a capability tool 302'd from a declared path to an undeclared one on
/// the SAME allowed host must be stopped by its own `path_prefixes`, or the
/// declared ∩ granted invariant holds everywhere except where an upstream
/// chooses. The hop check passes no credential name: credential *membership*
/// was settled on the initial request, and whether the header actually rides
/// a given hop is the same-origin rule's decision, not the declaration's.
/// Guest headers (#105) are settled the same way and for the same reason.
///
/// ## Guest headers travel exactly as far as the credential
///
/// One rule, not two: a header the caller attached rides while
/// `send_credential` holds and is dropped the moment the chain leaves its
/// origin. These are not secrets — the caller chose the values — so the
/// argument is not confidentiality but intent: `X-Goog-User-Project` was
/// meant for the host the caller named, and a redirect off-origin is exactly
/// where "meant for" stops being true. Sending a project id, a tenant, or an
/// API-version header onward to a destination an intermediary picked would be
/// the broker volunteering the caller's context to a stranger.
///
/// `max_response_bytes` bounds the READ of the final body, not just its
/// acceptance — `Some(n)` reads at most `n + 1` bytes, so an upstream cannot
/// make the broker buffer ten megabytes on the way to refusing one.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    start_url: &str,
    start_method: &str,
    body: Option<&[u8]>,
    credential_headers: &[(String, String)],
    guest_headers: &[(String, String)],
    policy: &EgressPolicy,
    grant: &CallerGrant,
    declaration: Option<&Declaration>,
    max_response_bytes: Option<usize>,
    binary: bool,
    caller: &str,
    refusals: &Arc<std::sync::Mutex<Vec<EgressRefusal>>>,
) -> Dispatched {
    // A bounded agent: a connector must not be able to park the evaluator on a
    // slow upstream, which would hold the trigger's lease for the duration.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(60)))
        // The two settings this whole function exists to change.
        .max_redirects(0)
        // With auto-follow off we have to read the `3xx` ourselves, and ureq's
        // default turns every non-2xx into an `Err` carrying the status but not
        // the headers — which would put `Location` out of reach. Switching it
        // off also fixes a smaller wrong: a 404 or a 429 used to reach the
        // caller as `502 {"error": "upstream: …"}`, indistinguishable from the
        // connection having failed. The broker's contract is to answer with the
        // response, so it now does.
        .http_status_as_error(false)
        .build()
        .into();

    let mut url = start_url.to_string();
    let mut method = start_method.to_string();
    let mut body = body.map(<[u8]>::to_vec);
    // Once any hop leaves the starting origin the credential is retired for the
    // rest of the chain, never to return. Comparing each hop against
    // `start_url` alone is not enough: an untrusted intermediary can answer
    // `A → 302 B → 302 A/<path it chose>`, and on that final hop
    // `same_origin(A, A)` is true, so the secret would be re-attached to a
    // request an intermediary shaped. Browsers and `curl --location` drop the
    // header for good once the chain leaves the origin; this matches them.
    let mut left_origin = false;
    // The cap bounds what is READ: ureq abandons the body at the limit and
    // reports it as `BodyExceedsLimit`, which surfaces as `BodyErr::TooLarge`
    // here so an overrun becomes a typed refusal — never an empty or
    // truncated body passed off as the upstream's answer.
    //
    // Every OTHER mid-body failure is typed too since #298. It used to map to
    // an empty string, which meant a non-UTF-8 body (a PDF, an image, any
    // binary attachment) came back as `{"status":200,"body":""}` with the
    // audit grain recording a 0-byte success. Text still refuses invalid
    // UTF-8; explicit artifact mode stores bounded exact bytes in CAS.
    let read_body = |resp: &mut ureq::http::Response<ureq::Body>| -> std::result::Result<Vec<u8>, BodyErr> {
        // `max_response_bytes` is the caller's EFFECTIVE ceiling, resolved in
        // `serve_one` (#339): an artifact always has one (declared, else
        // 1 MiB); text retains its old no-cap behavior for callers without a
        // capability declaration. Counted as it is read, so a chunked or
        // close-delimited body with no Content-Length is refused at cap + 1
        // exactly like a declared one.
        let cap = max_response_bytes.unwrap_or(usize::MAX - 1);
        let bytes = resp.body_mut().with_config().limit(cap as u64 + 1).read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => BodyErr::TooLarge,
                // An artifact interrupted mid-body (fewer bytes than its
                // Content-Length, a torn chunk) is a transport failure, not a
                // short success and not a text-decoding problem.
                other if binary => BodyErr::Transport(other.to_string()),
                other => BodyErr::NotText(other.to_string()),
            })?;
        if bytes.len() > cap { return Err(BodyErr::TooLarge); }
        if !binary {
            std::str::from_utf8(&bytes).map_err(|e| BodyErr::NotText(e.to_string()))?;
        }
        Ok(bytes)
    };

    // Inclusive: the initial request plus up to MAX_REDIRECT_HOPS follows.
    for hops in 0..=MAX_REDIRECT_HOPS as u32 {
        // The first hop is the URL the caller named and every layer above has
        // already cleared — the credential goes without a parse-dependent
        // detour. On follows, it rides only while the chain has never left the
        // starting origin (`same_origin` is then necessarily true too, but the
        // `left_origin` latch is what makes an A→B→A bounce fail closed).
        let send_credential =
            hops == 0 || (!left_origin && crate::egress::same_origin(start_url, &url));
        let result = perform(
            &agent,
            &method,
            &url,
            body.as_deref(),
            credential_headers,
            guest_headers,
            send_credential,
        );

        let mut resp = match result {
            Ok(r) => r,
            Err(e) => return Dispatched::Upstream(e.to_string()),
        };
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let media = resp.headers().get("content-type")
            .and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream")
            .split(';').next().unwrap_or("").trim();
        let mime = if media.len() <= 128 && media.contains('/') &&
            media.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'+' | b'.')) {
            media.to_string()
        } else {
            "application/octet-stream".to_string()
        };

        let Some(next_method) = redirect_method(status, &method) else {
            // Not a redirect, or one we deliberately do not follow. Either way
            // the caller gets the response as it stands.
            return match read_body(&mut resp) {
                Ok(text) => Dispatched::Answered {
                    status,
                    body: text,
                    mime,
                    final_url: url,
                    redirects: hops,
                    credential_sent: send_credential,
                },
                Err(BodyErr::TooLarge) => Dispatched::TooLarge { final_url: url },
                Err(BodyErr::NotText(detail)) => {
                    Dispatched::NotText { final_url: url, detail }
                }
                Err(BodyErr::Transport(detail)) => Dispatched::Upstream(format!(
                    "the response body was interrupted before it completed ({detail}) — \
                     refused rather than stored short"
                )),
            };
        };
        let Some(location) = location else {
            // A 3xx with no usable `Location` is not a redirect we can follow;
            // hand it back rather than invent a destination.
            return match read_body(&mut resp) {
                Ok(text) => Dispatched::Answered {
                    status,
                    body: text,
                    mime,
                    final_url: url,
                    redirects: hops,
                    credential_sent: send_credential,
                },
                Err(BodyErr::TooLarge) => Dispatched::TooLarge { final_url: url },
                Err(BodyErr::NotText(detail)) => {
                    Dispatched::NotText { final_url: url, detail }
                }
                Err(BodyErr::Transport(detail)) => Dispatched::Upstream(format!(
                    "the response body was interrupted before it completed ({detail}) — \
                     refused rather than stored short"
                )),
            };
        };
        let Some(next_url) = crate::egress::resolve_location(&url, &location) else {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.to_string(),
                    destination: location.clone(),
                    reason: "redirected to a Location that is not a resolvable http(s) URL".into(),
                },
            );
            return Dispatched::Refused {
                detail: format!(
                    "caller '{caller}' was redirected to {location:?}, which does not resolve to \
                     an http(s) URL — refused rather than guessed at"
                ),
            };
        };

        // THE check the old code did once and this does every hop.
        if let Err(e) = policy.permits(&next_url) {
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.to_string(),
                    destination: next_url.clone(),
                    // Worded apart from the initial refusal on purpose: "it
                    // aimed there" and "it was redirected there" are different
                    // stories for whoever reads the audit record.
                    reason: "redirect target outside the declared allowlist".into(),
                },
            );
            return Dispatched::Refused {
                detail: format!(
                    "caller '{caller}' was redirected from {url} and {e} — the redirect was not \
                     followed"
                ),
            };
        }
        // A 303 can turn a granted POST into a GET, and a 30x can turn a
        // granted GET into nothing else — but the grant is checked against
        // whatever we are about to issue, not against what was asked for.
        if !grant.permits_method(&next_method) {
            let denied = EgressDenied::Method { method: next_method.clone() };
            note_refusal(
                refusals,
                EgressRefusal {
                    caller: caller.to_string(),
                    destination: next_url.clone(),
                    reason: format!(
                        "redirect would issue {next_method}, which is not permitted for this caller"
                    ),
                },
            );
            return Dispatched::Refused {
                detail: format!("caller '{caller}' {denied} after a redirect"),
            };
        }
        // And the DECLARED half for a capability tool — a hop is a destination
        // the caller never named, and the declaration's host/path/method gates
        // apply to it exactly as they applied to the initial request. Without
        // this, a 302 on a declared host walks a module from its declared
        // `path_prefixes` to any endpoint the host-side grant tolerates.
        if let Some(d) = declaration {
            // The same private-space rule the initial URL got: a public,
            // policy-permitted start can 302 to the metadata service, and for
            // a capability caller under an unrestricted policy that hop needs
            // an explicit allowlist entry it does not have.
            if policy.is_unrestricted() && crate::egress::is_private_destination(&next_url) {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.to_string(),
                        destination: next_url.clone(),
                        reason: "redirected to a private or loopback destination without an \
                                 explicit --allow-host entry"
                            .into(),
                    },
                );
                return Dispatched::Refused {
                    detail: format!(
                        "caller '{caller}' was redirected from {url} to a private or loopback \
                         destination — the redirect was not followed"
                    ),
                };
            }
            if let Err(denied) = d.permits(&next_url, &next_method, None, &[]) {
                note_refusal(
                    refusals,
                    EgressRefusal {
                        caller: caller.to_string(),
                        destination: next_url.clone(),
                        reason: format!("redirect outside the declared capability: {denied}"),
                    },
                );
                return Dispatched::Refused {
                    detail: format!(
                        "caller '{caller}' was redirected from {url} and {denied} — the redirect \
                         was not followed"
                    ),
                };
            }
        }

        // A method that lost its body must not carry one forward.
        if next_method != method && next_method == "GET" {
            body = None;
        }
        // A hop to a different origin retires the credential permanently: even
        // if a later hop returns to the start origin, the chain has passed
        // through somewhere untrusted that chose where it goes next.
        if !crate::egress::same_origin(start_url, &next_url) {
            left_origin = true;
        }
        method = next_method;
        url = next_url;
    }

    note_refusal(
        refusals,
        EgressRefusal {
            caller: caller.to_string(),
            destination: url.clone(),
            reason: format!("redirect chain exceeded {MAX_REDIRECT_HOPS} hops"),
        },
    );
    Dispatched::Refused {
        detail: format!(
            "caller '{caller}' followed {MAX_REDIRECT_HOPS} redirects without reaching a final \
             response — the chain was abandoned at {url}"
        ),
    }
}

/// The method to use for the hop after `status`, or `None` when this is not a
/// redirect we follow. Mirrors ureq's rules; see [`dispatch`].
fn redirect_method(status: u16, method: &str) -> Option<String> {
    match status {
        // Retaining statuses keep the method — but a method that carries a
        // body is not resent, and DELETE is excluded deliberately (repeating a
        // delete against a new URL is not obviously what anyone meant).
        307 | 308 => match method {
            "GET" | "HEAD" => Some(method.to_string()),
            _ => None,
        },
        // The historical shapes: everything that is not already a read becomes
        // a GET, which is what curl and every browser do.
        301..=303 => match method {
            "GET" | "HEAD" => Some(method.to_string()),
            _ => Some("GET".to_string()),
        },
        _ => None,
    }
}

/// One HTTP call. ureq 3 types body-carrying and body-less builders
/// differently, so the two shapes are dispatched separately rather than
/// unified behind a cast.
fn perform(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    credential_headers: &[(String, String)],
    guest_headers: &[(String, String)],
    send_credential: bool,
) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    // Both sets ride or neither does — see `dispatch`'s "travel exactly as far
    // as the credential". The credential goes on LAST so that even if the two
    // ever named the same header, the broker's value is the one that survives;
    // the guest cannot reach these names at all (`is_broker_owned_header`, and
    // a `Credential::Header` name is refused beside it), so this is a belt on
    // top of braces rather than the guarantee itself.
    let credential_headers: &[(String, String)] =
        if send_credential { credential_headers } else { &[] };
    let guest_headers: &[(String, String)] = if send_credential { guest_headers } else { &[] };
    match method {
        "POST" | "PUT" | "PATCH" => {
            let mut b = match method {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                _ => agent.patch(url),
            };
            for (k, v) in guest_headers.iter().chain(credential_headers) {
                b = b.header(k, v);
            }
            b.send(body.unwrap_or(b""))
        }
        _ => {
            let mut b = match method {
                "HEAD" => agent.head(url),
                "DELETE" => agent.delete(url),
                _ => agent.get(url),
            };
            for (k, v) in guest_headers.iter().chain(credential_headers) {
                b = b.header(k, v);
            }
            b.call()
        }
    }
}

/// Best-effort: read and discard up to `content_length` bytes (capped at
/// [`MAX_BODY`]) still pending on the socket.
///
/// A refusal that writes its response and drops the connection WITHOUT
/// reading a body the caller already started sending leaves those bytes
/// queued in the kernel receive buffer. Closing a socket over unread data
/// sends an RST instead of a clean FIN — which can surface to the caller as a
/// raw `ConnectionReset` on their own write, burying the 401/403 JSON body
/// under an opaque I/O error instead of a readable refusal. Draining first
/// turns that into an ordinary, parseable response every time. The cap
/// matters even here: a caller presenting a bad token gets no free pass to
/// make us read an unbounded body.
fn drain_body(reader: &mut BufReader<TcpStream>, content_length: usize) {
    let mut discard = vec![0u8; content_length.min(MAX_BODY)];
    let _ = reader.read_exact(&mut discard);
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} \r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Answer with raw bytes rather than JSON (#106).
///
/// A blob is arbitrary bytes — a PDF, an image, a zip — and JSON cannot carry
/// those without base64, which would cost a third of the size on the wire and,
/// worse, oblige every guest module to carry a base64 decoder to read its own
/// attachment. Unlike an HTTP response there is nothing else to report: no
/// status to relay, no headers, just the bytes at an address that already
/// names them. So success is `200` plus the body, and every failure is JSON
/// with an `error` — which the sandbox distinguishes by status, not by
/// sniffing the payload.
fn respond_bytes(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 \r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(expected: Vec<u8>, response: Vec<u8>, mime: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/artifact", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" { break; }
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap();
                }
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            assert_eq!(sent, expected);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
            let _ = stream.write_all(&response); // overrun tests abandon the read early
        });
        (url, thread)
    }

    #[test]
    fn binary_request_and_response_are_byte_exact_and_audited_without_bytes() {
        for (bytes, mime) in [
            (vec![0, 0x80, 0xff, 10], "application/octet-stream"),
            (b"%PDF-1.7\n\0\x80\xff".to_vec(), "application/pdf"),
            (b"PK\x03\x04\0\xff\x80".to_vec(), "application/zip"),
        ] {
            let (url, server) = fixture(bytes.clone(), bytes.clone(), mime);
            let origin = url.trim_end_matches("/artifact");
            let broker = Broker::start(policy(&[origin]), BTreeMap::new(), grants(&[]), "RUN-E022").unwrap();
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(AreevFacade::new(areev_store::Areev::open(
                dir.path().join("m.db").to_str().unwrap()).unwrap()));
            let request_ref = store.with_store(|m| m.put_blob(&bytes)).unwrap();
            broker.bind_artifact_store(Arc::clone(&store));
            broker.declare("", Declaration::parse(&serde_json::json!([
                {"blob":{"read":true}},
                {"http":{"hosts":[origin],"methods":["POST"],"headers":["Content-Type"]}}
            ])).unwrap(), CapabilityLimits::default());
            let (status, answer) = call(&broker, serde_json::json!({
                "url": url, "method": "POST_ARTIFACT", "response_mode": "artifact",
                "body_ref": request_ref,
                "content_type": mime
            }));
            assert_eq!(status, 200, "{answer}");
            server.join().unwrap();
            assert_eq!(answer["status"], 200);
            assert_eq!(answer["mime"], mime);
            assert_eq!(answer["bytes"], bytes.len());
            assert_eq!(answer["sha256"], digest_bytes(&bytes));
            assert_eq!(answer["ref"], request_ref);
            assert_eq!(store.with_store(|m| m.get_blob(answer["ref"].as_str().unwrap())).unwrap(), bytes);
            assert!(answer.get("body").is_none());
            let calls = broker.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].request_digest.as_deref(), Some(digest_bytes(&bytes).as_str()));
            assert_eq!(calls[0].response_digest, digest_bytes(&bytes));
            assert_eq!(calls[0].response_bytes, bytes.len());
            assert_eq!(calls[0].response_ref.as_deref(), Some(request_ref.as_str()));
            store.with_store(|m| {
                crate::journal::write_egress_call(m, "binary", &calls[0], 1_788_134_400_000,
                    "user:test", None).unwrap();
                assert_eq!(m.gc_blobs().unwrap(), 0);
                assert_eq!(m.get_blob(&request_ref).unwrap(), bytes);
            });
        }
    }

    #[test]
    fn unsupported_modes_and_malformed_binary_requests_fail_before_dispatch() {
        let broker = Broker::start(policy(&["http://127.0.0.1:1"]), BTreeMap::new(), grants(&[]), "RUN-E022").unwrap();
        for req in [
            serde_json::json!({"response_mode":"future"}),
            serde_json::json!({"body_ref":"cas://sha256:abc"}),
            serde_json::json!({"body":"x", "body_ref":"cas://sha256:abc", "content_type":"application/pdf"}),
            serde_json::json!({"url":"http://127.0.0.1:1/x", "method":"POST_ARTIFACT"}),
            serde_json::json!({"url":"http://127.0.0.1:1/x", "method":"POST", "body_ref":"cas://sha256:abc", "content_type":"application/pdf"}),
        ] {
            let (status, answer) = call(&broker, req);
            assert_eq!(status, 400, "{answer}");
        }
        assert!(broker.calls().is_empty());
        // The old peer's method whitelist has GET/HEAD/DELETE/POST/PUT/PATCH
        // only; the marker therefore cannot issue an empty upload there.
        for marker in ["POST_ARTIFACT", "PUT_ARTIFACT", "PATCH_ARTIFACT"] {
            assert!(!matches!(marker, "GET" | "HEAD" | "DELETE" | "POST" | "PUT" | "PATCH"));
        }
        let (status, answer) = call(&broker, serde_json::json!({
            "url":"http://127.0.0.1:1/artifact", "response_mode":"artifact"
        }));
        assert_eq!(status, 503, "{answer}");
    }

    #[test]
    fn downloaded_artifact_uses_the_memorys_encryption_key() {
        let bytes = b"SECRET-PDF-BYTES-\x00\x80\xff".to_vec();
        let (url, server) = fixture(Vec::new(), bytes.clone(), "application/pdf");
        let origin = url.trim_end_matches("/artifact");
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("encrypted.db");
        let opts = areev_store::AreevOptions {
            encryption_key: Some([42u8; 32]), ..Default::default()
        };
        let store = Arc::new(AreevFacade::new(areev_store::Areev::open_with(
            db.to_str().unwrap(), opts).unwrap()));
        let broker = Broker::start(policy(&[origin]), BTreeMap::new(), grants(&[]), "RUN-E022").unwrap();
        broker.bind_artifact_store(Arc::clone(&store));
        let (status, answer) = call(&broker, serde_json::json!({"url":url,"response_mode":"artifact"}));
        assert_eq!(status, 200, "{answer}");
        server.join().unwrap();
        let uri = answer["ref"].as_str().unwrap();
        assert_eq!(store.with_store(|m| m.get_blob(uri)).unwrap(), bytes);
        let hex = uri.strip_prefix("cas://sha256:").unwrap();
        let sidecar = db.with_file_name("encrypted.db.blobs")
            .join(&hex[..2]).join(&hex[2..]);
        let stored = std::fs::read(sidecar).unwrap();
        assert!(!stored.windows(bytes.len()).any(|w| w == bytes));
    }

    #[test]
    fn binary_overrun_is_a_typed_error_not_a_empty_success() {
        let bytes = vec![0x80; MAX_BODY + 1];
        let (url, server) = fixture(Vec::new(), bytes, "application/pdf");
        let origin = url.trim_end_matches("/artifact");
        let broker = Broker::start(policy(&[origin]), BTreeMap::new(), grants(&[]), "RUN-E022").unwrap();
        bind_test_store(&broker);
        let (status, answer) = call(&broker, serde_json::json!({"url":url,"response_mode":"artifact"}));
        server.join().unwrap();
        assert_eq!(status, 403, "{answer}");
        assert_eq!(answer["code"], "RUN-E022");
        assert!(broker.calls().is_empty());
    }

    #[test]
    fn interrupted_binary_response_is_not_a_success() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{origin}/artifact");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" { break; }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n\x00\x80").unwrap();
        });
        let broker = Broker::start(policy(&[&origin]), BTreeMap::new(), grants(&[]), "RUN-E022").unwrap();
        bind_test_store(&broker);
        let (status, answer) = call(&broker, serde_json::json!({"url":url,"response_mode":"artifact"}));
        server.join().unwrap();
        assert_eq!(status, 502, "{answer}");
        assert!(answer["error"].as_str().unwrap().starts_with("upstream:"));
        assert!(broker.calls().is_empty());
    }

    // ---- #339: declared artifact limits above 1 MiB -----------------------

    /// A memory the test keeps alive (and can read back) for as long as it
    /// holds the returned directory.
    fn kept_store() -> (tempfile::TempDir, Arc<AreevFacade>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AreevFacade::new(areev_store::Areev::open(
            dir.path().join("m.db").to_str().unwrap()).unwrap()));
        (dir, store)
    }

    /// An upstream that reads one request (headers + Content-Length body),
    /// hands the stream to `answer`, and returns the body it received.
    fn upstream<F>(answer: F) -> (String, std::thread::JoinHandle<Vec<u8>>)
    where
        F: FnOnce(&mut TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() { break; }
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap();
                }
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            answer(&mut stream);
            sent
        });
        (origin, thread)
    }

    /// How an upstream frames its body.
    #[derive(Clone, Copy, Debug)]
    enum Framing { Length, Chunked, CloseDelimited }

    fn framed(body: Vec<u8>, framing: Framing) -> impl FnOnce(&mut TcpStream) + Send + 'static {
        move |s: &mut TcpStream| {
            // Write errors are expected on the overrun cases: the broker
            // abandons the read at its ceiling and closes.
            let _ = match framing {
                Framing::Length => write!(s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n", body.len())
                    .and_then(|_| s.write_all(&body)),
                Framing::Chunked => write!(s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\n\
                     Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                    .and_then(|_| {
                        for chunk in body.chunks(1000) {
                            write!(s, "{:x}\r\n", chunk.len())?;
                            s.write_all(chunk)?;
                            s.write_all(b"\r\n")?;
                        }
                        s.write_all(b"0\r\n\r\n")
                    }),
                Framing::CloseDelimited => write!(s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nConnection: close\r\n\r\n")
                    .and_then(|_| s.write_all(&body)),
            };
            let _ = s.flush();
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
    }

    /// A broker whose default caller declared GET (+ blob-read POST uploads)
    /// against `origin`, with `limits`, bound to `store`.
    fn artifact_broker(
        origin: &str,
        limits: CapabilityLimits,
        store: &Arc<AreevFacade>,
        credentials: BTreeMap<String, CredentialSource>,
    ) -> Broker {
        let creds: Vec<String> = credentials.keys().cloned().collect();
        let broker = Broker::start(
            policy(&[origin]),
            credentials,
            grants(&creds.iter().map(String::as_str).collect::<Vec<_>>()),
            "RUN-E022",
        )
        .unwrap();
        broker.bind_artifact_store(Arc::clone(store));
        broker.declare("", Declaration::parse(&serde_json::json!([
            {"blob": {"read": true}},
            {"http": {"hosts": [origin], "methods": ["GET", "POST"],
                      "headers": ["Content-Type"], "credentials": creds}}
        ])).unwrap(), limits);
        broker
    }

    fn limits(response: usize, request: usize) -> CapabilityLimits {
        CapabilityLimits { max_response_bytes: response, max_request_bytes: request, ..Default::default() }
    }

    /// 2 MiB + 13 bytes that are not UTF-8, with a recognisable marker inside
    /// so "raw bytes stayed out of the journal" is checkable.
    fn document_fixture() -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..2 * 1024 * 1024 + 13u32)
            .map(|i| 0x80 | (i.wrapping_mul(31) % 127) as u8)
            .collect();
        bytes[0] = 0xff;
        let marker = b"RAW-DOCUMENT-MARKER-339";
        bytes[64..64 + marker.len()].copy_from_slice(marker);
        assert!(std::str::from_utf8(&bytes).is_err());
        bytes
    }

    const SECRET: &str = "sk-live-339-never-journaled-value";

    #[test]
    fn a_25_mib_declaration_downloads_a_2_mib_document_exactly_and_journals_no_secret_or_byte() {
        let bytes = document_fixture();
        let (origin, server) = upstream(framed(bytes.clone(), Framing::Length));
        let (_dir, store) = kept_store();
        let creds: BTreeMap<String, CredentialSource> =
            [("vendor".to_string(), Credential::Bearer(SECRET.into()).into())].into();
        let broker = artifact_broker(&origin, limits(26_214_400, DEFAULT_TRANSFER), &store, creds);
        let (status, answer) = call(&broker, serde_json::json!({
            "url": format!("{origin}/statement.pdf"), "response_mode": "artifact",
            "credential": "vendor"
        }));
        server.join().unwrap();
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["bytes"], 2 * 1024 * 1024 + 13);
        assert_eq!(answer["sha256"], digest_bytes(&bytes));
        let uri = answer["ref"].as_str().unwrap().to_string();
        let stored = store.with_store(|m| m.get_blob(&uri)).unwrap();
        assert_eq!(stored.len(), bytes.len());
        assert_eq!(digest_bytes(&stored), digest_bytes(&bytes));
        assert_eq!(stored, bytes);

        // The audit record: the credential by NAME, the body by digest and
        // address — neither the secret nor a single raw byte.
        let calls = broker.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].credential.as_deref(), Some("vendor"));
        let h = store.with_store(|m| {
            crate::journal::write_egress_call(m, "r339", &calls[0], 1_788_134_400_000,
                "user:test", None)
        }).unwrap();
        let grain = store.with_store(|m| m.get(&h)).unwrap();
        let recorded = format!("{:?}", grain.fields);
        assert!(!recorded.contains(SECRET), "credential value journaled: {recorded}");
        assert!(!recorded.contains("RAW-DOCUMENT-MARKER-339"), "raw bytes journaled");
        assert!(recorded.contains(&digest_bytes(&bytes)), "{recorded}");
        assert!(recorded.len() < 4096, "the record is metadata, not the document");
        assert!(format!("{calls:?}").len() < 4096);
    }

    #[test]
    fn a_1_mib_declaration_refuses_the_same_document_naming_1048576() {
        let bytes = document_fixture();
        let (origin, server) = upstream(framed(bytes, Framing::Length));
        let (_dir, store) = kept_store();
        let broker = artifact_broker(&origin, limits(1_048_576, DEFAULT_TRANSFER), &store, BTreeMap::new());
        let (status, answer) = call(&broker, serde_json::json!({
            "url": format!("{origin}/statement.pdf"), "response_mode": "artifact"
        }));
        server.join().unwrap();
        assert_eq!(status, 403, "{answer}");
        assert_eq!(answer["code"], "RUN-E022");
        let error = answer["error"].as_str().unwrap();
        assert!(error.contains("1048576-byte"), "names the effective limit: {error}");
        assert!(answer.get("ref").is_none() && answer.get("bytes").is_none(), "no partial success");
        assert!(broker.calls().is_empty(), "an overrun is not a call that succeeded");
        let refusals = broker.refusals();
        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].reason.contains("1048576-byte"), "{:?}", refusals[0]);
        // The same record that goes in the journal carries no body either.
        let h = store.with_store(|m| {
            crate::journal::write_egress_refusal(m, "r339", &refusals[0], 1_788_134_400_000,
                "user:test", None)
        }).unwrap();
        let recorded = format!("{:?}", store.with_store(|m| m.get(&h)).unwrap().fields);
        assert!(!recorded.contains("RAW-DOCUMENT-MARKER-339"));
    }

    #[test]
    fn an_undeclared_artifact_keeps_the_1_mib_default_and_says_so() {
        // No declaration at all (a connector): the default is the effective
        // ceiling, and the refusal names it rather than 0 or the hard max.
        let (origin, server) = upstream(framed(vec![0x80; DEFAULT_TRANSFER + 1], Framing::Length));
        let broker = Broker::start(policy(&[&origin]), BTreeMap::new(), grants(&[]), "RUN-E022").unwrap();
        let (_dir, store) = kept_store();
        broker.bind_artifact_store(store);
        let (status, answer) = call(&broker, serde_json::json!({
            "url": format!("{origin}/a"), "response_mode": "artifact"
        }));
        server.join().unwrap();
        assert_eq!(status, 403, "{answer}");
        assert!(answer["error"].as_str().unwrap().contains("1048576-byte"), "{answer}");
    }

    #[test]
    fn the_ceiling_is_exact_at_the_limit_and_refuses_limit_plus_one_in_every_framing() {
        const LIMIT: usize = 4096;
        for framing in [Framing::Length, Framing::Chunked, Framing::CloseDelimited] {
            for (size, admitted) in [(LIMIT, true), (LIMIT + 1, false)] {
                let body: Vec<u8> = (0..size).map(|i| 0x80 | (i % 97) as u8).collect();
                let (origin, server) = upstream(framed(body.clone(), framing));
                let (_dir, store) = kept_store();
                let broker = artifact_broker(&origin, limits(LIMIT, DEFAULT_TRANSFER), &store, BTreeMap::new());
                let (status, answer) = call(&broker, serde_json::json!({
                    "url": format!("{origin}/x"), "response_mode": "artifact"
                }));
                server.join().unwrap();
                if admitted {
                    assert_eq!(status, 200, "{framing:?} at the limit: {answer}");
                    assert_eq!(answer["bytes"], LIMIT);
                    let got = store.with_store(|m| m.get_blob(answer["ref"].as_str().unwrap())).unwrap();
                    assert_eq!(got, body, "{framing:?}");
                } else {
                    assert_eq!(status, 403, "{framing:?} at limit + 1: {answer}");
                    assert_eq!(answer["code"], "RUN-E022");
                    assert!(answer["error"].as_str().unwrap().contains("4096-byte"), "{answer}");
                    assert!(broker.calls().is_empty(), "{framing:?}");
                }
            }
        }
    }

    #[test]
    fn an_interrupted_artifact_fails_rather_than_succeeding_short() {
        let cases: [(&str, &[u8]); 2] = [
            // Declares 5000 bytes, delivers 100, closes.
            ("length", b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n"),
            // A chunk announced at 0x1000 bytes and torn after 100.
            ("chunked", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1000\r\n"),
        ];
        for (name, head) in cases {
            let head = head.to_vec();
            let (origin, server) = upstream(move |s| {
                let _ = s.write_all(&head);
                let _ = s.write_all(&[0x80; 100]);
                let _ = s.flush();
                let _ = s.shutdown(std::net::Shutdown::Both);
            });
            let (_dir, store) = kept_store();
            let broker = artifact_broker(&origin, limits(26_214_400, DEFAULT_TRANSFER), &store, BTreeMap::new());
            let (status, answer) = call(&broker, serde_json::json!({
                "url": format!("{origin}/x"), "response_mode": "artifact"
            }));
            server.join().unwrap();
            assert_eq!(status, 502, "{name}: {answer}");
            let error = answer["error"].as_str().unwrap();
            assert!(error.contains("interrupted"), "{name}: {error}");
            assert!(answer.get("ref").is_none(), "{name}");
            assert!(broker.calls().is_empty(), "{name}");
        }
    }

    #[test]
    fn a_declared_16_mib_upload_is_sent_exactly() {
        const SIXTEEN: usize = 16 * 1024 * 1024;
        let bytes: Vec<u8> = (0..SIXTEEN).map(|i| 0x80 | (i.wrapping_mul(7) % 127) as u8).collect();
        let (origin, server) = upstream(|s| {
            let _ = s.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        let (_dir, store) = kept_store();
        let request_ref = store.with_store(|m| m.put_blob(&bytes)).unwrap();
        let broker = artifact_broker(&origin, limits(DEFAULT_TRANSFER, SIXTEEN), &store, BTreeMap::new());
        let (status, answer) = call(&broker, serde_json::json!({
            "url": format!("{origin}/upload"), "method": "POST_ARTIFACT",
            "body_ref": request_ref, "content_type": "application/pdf"
        }));
        let received = server.join().unwrap();
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["status"], 201);
        assert_eq!(received.len(), SIXTEEN);
        assert_eq!(digest_bytes(&received), digest_bytes(&bytes));
        let calls = broker.calls();
        assert_eq!(calls[0].request_digest.as_deref(), Some(digest_bytes(&bytes).as_str()));
        assert_eq!(broker.blob_reads()[0].bytes, SIXTEEN);
    }

    #[test]
    fn an_over_ceiling_upload_is_refused_before_the_upstream_sees_a_byte() {
        // Declared 16 MiB, asked to send 16 MiB + 1; and undeclared (1 MiB
        // default), asked to send 1 MiB + 1. Neither connects upstream.
        for (declared, size) in [
            (16 * 1024 * 1024, 16 * 1024 * 1024 + 1),
            (DEFAULT_TRANSFER, DEFAULT_TRANSFER + 1),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let (_dir, store) = kept_store();
            let request_ref = store.with_store(|m| m.put_blob(&vec![0xfe; size])).unwrap();
            let broker = artifact_broker(&origin, limits(DEFAULT_TRANSFER, declared), &store, BTreeMap::new());
            let (status, answer) = call(&broker, serde_json::json!({
                "url": format!("{origin}/upload"), "method": "POST_ARTIFACT",
                "body_ref": request_ref, "content_type": "application/pdf"
            }));
            assert_eq!(status, 413, "{answer}");
            assert_eq!(answer["code"], "RUN-E022");
            let error = answer["error"].as_str().unwrap();
            assert!(error.contains(&format!("{declared}-byte")), "names the effective limit: {error}");
            assert!(
                matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "the upstream was never connected to"
            );
            assert!(broker.calls().is_empty());
            assert!(broker.blob_reads().is_empty(), "nothing was handed on");
            assert_eq!(broker.refusals().len(), 1);
        }
    }

    #[test]
    fn out_of_range_limits_are_refused_before_any_upstream_io_never_clamped() {
        for (l, req) in [
            (limits(0, DEFAULT_TRANSFER), serde_json::json!({"response_mode": "artifact"})),
            (limits(MAX_TRANSFER + 1, DEFAULT_TRANSFER), serde_json::json!({"response_mode": "artifact"})),
            (limits(DEFAULT_TRANSFER, MAX_TRANSFER + 1), serde_json::json!({
                "method": "POST_ARTIFACT", "content_type": "application/pdf"})),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let (_dir, store) = kept_store();
            let r = store.with_store(|m| m.put_blob(b"%PDF")).unwrap();
            let broker = artifact_broker(&origin, l, &store, BTreeMap::new());
            let mut req = req;
            req["url"] = serde_json::json!(format!("{origin}/x"));
            if req.get("method").is_some() { req["body_ref"] = serde_json::json!(r); }
            let (status, answer) = call(&broker, req);
            assert_eq!(status, 400, "{answer}");
            assert_eq!(answer["code"], "RUN-E028");
            assert!(answer["error"].as_str().unwrap().starts_with("RUN-E028: "), "{answer}");
            assert!(matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock));
        }
    }

    #[test]
    fn text_mode_keeps_its_contract_under_a_large_declaration() {
        // Text is unchanged: a declared ceiling still bounds it, and a body
        // that is not UTF-8 is still refused as such, not stored.
        let (origin, server) = upstream(framed(b"plain text".to_vec(), Framing::Length));
        let (_dir, store) = kept_store();
        let broker = artifact_broker(&origin, limits(26_214_400, DEFAULT_TRANSFER), &store, BTreeMap::new());
        let (status, answer) = call(&broker, serde_json::json!({"url": format!("{origin}/t")}));
        server.join().unwrap();
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["body"], "plain text");
        assert!(answer.get("ref").is_none());
    }

    fn bind_test_store(broker: &Broker) {
        // The broker retains the facade after the TempDir is dropped; the
        // interrupted/overrun cases never write it.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AreevFacade::new(areev_store::Areev::open(
            dir.path().join("m.db").to_str().unwrap()).unwrap()));
        broker.bind_artifact_store(store);
    }

    fn call(broker: &Broker, req: serde_json::Value) -> (u16, serde_json::Value) {
        call_as(broker, "connector", req)
    }

    /// Call presenting `caller`'s token, or a bogus one if it has no grant.
    fn call_as(broker: &Broker, caller: &str, req: serde_json::Value) -> (u16, serde_json::Value) {
        let token = broker.token_for(caller).unwrap_or("not-a-real-token").to_string();
        call_with_token(broker, &token, req)
    }

    fn call_with_token(
        broker: &Broker,
        token: &str,
        req: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let addr = broker.url().trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = req.to_string();
        let head = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nX-Areev-Egress-Token: {token}\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body.as_bytes()).unwrap();
        s.flush().unwrap();

        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status: u16 = raw
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("{}");
        (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
    }

    /// The connector-shaped setup the existing cases assume: one caller, all
    /// methods, every configured credential.
    fn grants(creds: &[&str]) -> EgressGrants {
        let g = creds.iter().fold(
            CallerGrant::new().method("GET").method("POST").method("DELETE"),
            |g, c| g.credential(c),
        );
        EgressGrants::new().default_for_all(g)
    }

    fn policy(entries: &[&str]) -> EgressPolicy {
        EgressPolicy::from_config(Some(&serde_json::json!({
            "int:allowed_outbound_hosts": entries
        })))
        .unwrap()
    }

    #[test]
    fn a_disallowed_destination_is_refused_before_any_request_is_made() {
        // The point: refusal happens here, so a connector aiming somewhere it
        // should not never gets a socket to that host at all.
        let b = Broker::start(policy(&["https://api.github.com"]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, body) = call(&b, serde_json::json!({ "url": "https://evil.com/steal" }));
        assert_eq!(status, 403);
        assert_eq!(body["code"], "TRG-E009");
        let refusals = b.refusals();
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].destination, "https://evil.com/steal");
        assert!(refusals[0].reason.contains("allowlist"), "{:?}", refusals[0]);
    }

    #[test]
    fn a_caller_cannot_name_a_credential_its_grant_does_not_cover() {
        // Naming a credential is not the same as being allowed to use it.
        let b = Broker::start(
            policy(&["https://api.github.com"]),
            BTreeMap::new(),
            grants(&[]),
            "TRG-E009",
        )
        .unwrap();
        let (status, body) = call(
            &b,
            serde_json::json!({ "url": "https://api.github.com/x", "credential": "gmail" }),
        );
        assert_eq!(status, 403);
        assert!(body["error"].as_str().unwrap().contains("may not use credential"), "{body}");
    }

    #[test]
    fn a_granted_credential_that_the_host_never_configured_is_a_client_error() {
        // Distinct from the case above: the grant permits the name, but no
        // value was configured. That is the host's mistake, not the caller's
        // overreach, and the status says which.
        let b = Broker::start(
            policy(&["https://api.github.com"]),
            BTreeMap::new(),
            grants(&["gmail"]),
            "TRG-E009",
        )
        .unwrap();
        let (status, body) = call(
            &b,
            serde_json::json!({ "url": "https://api.github.com/x", "credential": "gmail" }),
        );
        assert_eq!(status, 400);
        assert!(body["error"].as_str().unwrap().contains("no credential named"), "{body}");
    }

    #[test]
    fn a_malformed_request_is_a_client_error_not_a_panic() {
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, _) = call(&b, serde_json::json!("not an egress request"));
        assert_eq!(status, 400);
    }

    #[test]
    fn a_caller_with_no_token_is_refused_before_its_body_is_parsed() {
        // Loopback is not an authorization: without a token, any process on
        // the box could spend the credentials this broker holds.
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let addr = b.url().trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        let body = "not json";
        s.write_all(
            format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                .as_bytes(),
        )
        .unwrap();
        s.flush().unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        assert!(raw.contains("401"), "{raw}");
    }

    #[test]
    fn a_forged_token_is_refused() {
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, _) =
            call_with_token(&b, "0".repeat(48).as_str(), serde_json::json!({ "url": "https://x/" }));
        assert_eq!(status, 401);
    }

    /// A refusal must not corrupt the caller's own write with an RST (found
    /// while releasing 1.3.1: a `ConnectionReset` here once, under heavy
    /// concurrent-build load).
    ///
    /// The tiny bodies elsewhere in this file fit entirely inside the OS
    /// socket buffers, so `write_all` never blocks and completes before the
    /// server has a chance to close — the race that causes an RST only shows
    /// up under enough scheduling delay to matter, which is exactly why it
    /// took heavy system load to surface and why a normal CI run would not
    /// reliably catch a regression here. A body large enough to force real
    /// TCP backpressure reproduces the same race on every run, load or not:
    /// with the server closing before draining, this `write_all` fails with
    /// `ConnectionReset`; with it draining first, the write always completes
    /// and the 401 is always readable. Verified: this test fails
    /// deterministically (no stress needed) against the code before the
    /// `drain_body` fix, on the very first run.
    #[test]
    fn a_refusal_drains_the_body_so_a_large_write_never_resets() {
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        // Comfortably under MAX_BODY (drain_body covers the whole thing) and
        // comfortably over typical default OS socket buffer sizes, so the
        // write backpressures for real rather than completing instantly.
        let pad = "a".repeat(900_000);
        let (status, _) =
            call_with_token(&b, "0".repeat(48).as_str(), serde_json::json!({ "pad": pad }));
        assert_eq!(status, 401);
    }

    #[test]
    fn a_grant_naming_no_method_may_only_read() {
        // Connectors read; tools write. The write verb is always something
        // someone decided to allow.
        let b = Broker::start(
            EgressPolicy::unrestricted(),
            BTreeMap::new(),
            EgressGrants::new().grant("reader", CallerGrant::new()),
            "TRG-E009",
        )
        .unwrap();
        let (status, body) = call_as(
            &b,
            "reader",
            serde_json::json!({ "url": "https://example.com/x", "method": "POST" }),
        );
        assert_eq!(status, 403);
        assert!(body["error"].as_str().unwrap().contains("not permitted"), "{body}");
    }

    #[test]
    fn one_callers_token_does_not_buy_anothers_scope() {
        // Two tools, one broker, one port: the token is what tells them apart.
        let b = Broker::start(
            EgressPolicy::unrestricted(),
            BTreeMap::new(),
            EgressGrants::new()
                .grant("writer", CallerGrant::new().method("POST").credential("zoho"))
                .grant("reader", CallerGrant::new()),
            "TRG-E009",
        )
        .unwrap();
        assert_ne!(b.token_for("writer").unwrap(), b.token_for("reader").unwrap());

        // The reader presenting its own token cannot POST...
        let (status, _) = call_as(
            &b,
            "reader",
            serde_json::json!({ "url": "https://example.com/x", "method": "POST" }),
        );
        assert_eq!(status, 403);

        // ...and cannot reach for the writer's credential either.
        let (status, _) = call_as(
            &b,
            "reader",
            serde_json::json!({ "url": "https://example.com/x", "credential": "zoho" }),
        );
        assert_eq!(status, 403);
    }

    #[test]
    fn a_caller_with_no_grant_at_all_gets_no_token() {
        let b = Broker::start(
            EgressPolicy::unrestricted(),
            BTreeMap::new(),
            EgressGrants::new().grant("writer", CallerGrant::new()),
            "TRG-E009",
        )
        .unwrap();
        assert!(b.token_for("nobody").is_none(), "an ungranted caller must not see the broker");
    }

    #[test]
    fn the_broker_binds_loopback_only() {
        // A service that holds credentials must not be reachable off-box.
        let b = Broker::start(policy(&[]), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        assert!(b.url().starts_with("http://127.0.0.1:"), "{}", b.url());
    }

    #[test]
    fn credentials_come_from_a_named_variable_never_a_literal() {
        std::env::set_var("AREEV_TEST_BROKER_TOKEN", "s3cret");
        let c = Credential::bearer_from_env("AREEV_TEST_BROKER_TOKEN").unwrap();
        std::env::remove_var("AREEV_TEST_BROKER_TOKEN");
        assert!(matches!(c, Credential::Bearer(v) if v == "s3cret"));

        // Unset and empty both refuse: a broker that silently attaches nothing
        // sends unauthenticated requests that fail confusingly upstream.
        assert!(Credential::bearer_from_env("AREEV_TEST_BROKER_ABSENT").is_err());
        std::env::set_var("AREEV_TEST_BROKER_EMPTY", "   ");
        assert!(Credential::bearer_from_env("AREEV_TEST_BROKER_EMPTY").is_err());
        std::env::remove_var("AREEV_TEST_BROKER_EMPTY");
    }

    #[test]
    fn an_unrestricted_policy_still_brokers_rather_than_handing_over_the_token() {
        // Even with no allowlist, the credential stays here — the connector
        // gets a URL, not a secret.
        let b = Broker::start(EgressPolicy::unrestricted(), BTreeMap::new(), grants(&[]), "TRG-E009").unwrap();
        let (status, _) = call(&b, serde_json::json!({ "url": "https://anywhere.example/" }));
        assert_ne!(status, 403, "an absent allowlist does not refuse");
    }
}
