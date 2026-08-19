//! Per-request credentials for the LLM backends.
//!
//! Every backend used to hold a `String` read once from the environment, which
//! excludes every cloud-native auth model: Vertex AI with Application Default
//! Credentials, Azure managed identity, AWS SigV4. Those exist precisely so
//! that no key sits on disk — many organizations have an org policy that
//! blocks service-account key creation outright — so "read a static key from
//! an env var" is not a smaller version of them, it is the thing they forbid.
//!
//! The seam is deliberately narrow: a credential mints the value that goes in
//! the auth header, and may refresh it. Everything else — which header, which
//! endpoint — stays with the backend.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use areev_loop::{Error, Result};

/// A source of the value a backend puts in its auth header.
///
/// Implementations must be cheap on the hot path: `token()` is called per
/// request, so anything with an expiry caches until shortly before it.
pub trait Credential: Send + Sync {
    /// The current bearer token / API key. May perform I/O to refresh.
    fn token(&self) -> Result<String>;

    /// Short label for diagnostics. Never includes key material.
    fn kind(&self) -> &'static str;
}

/// A fixed key, read once — the historical behaviour, unchanged.
pub struct StaticKey(String);

impl StaticKey {
    pub fn new(key: impl Into<String>) -> Self {
        StaticKey(key.into())
    }
}

impl Credential for StaticKey {
    fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
    fn kind(&self) -> &'static str {
        "static"
    }
}

/// Google Application Default Credentials, for workloads that must not hold a
/// key.
///
/// Two sources, tried in order, matching what ADC actually does on a GCP
/// workload and on a developer machine:
///
/// 1. **The metadata server** (`GCE_METADATA_HOST`, default
///    `169.254.169.254`) — the attached service account of a Cloud Run / GKE /
///    GCE workload. This is workload identity: no key material anywhere.
/// 2. **The gcloud well-known file** (`GOOGLE_APPLICATION_CREDENTIALS`, else
///    `~/.config/gcloud/application_default_credentials.json`) when it holds
///    an `authorized_user` — a refresh token exchanged at Google's OAuth
///    endpoint.
///
/// **Deliberately NOT supported: `service_account` key JSON.** Honouring it
/// means signing a JWT with RS256, i.e. pulling an RSA implementation into a
/// tree whose stated policy is dependency-light — and the deployments this
/// exists for are exactly the ones whose org policy forbids creating such keys
/// in the first place. A `service_account` file is refused by name, with the
/// two supported paths in the message, rather than failing as "no credentials".
///
/// Tokens are cached until 60s before expiry, so a normal turn pays no
/// refresh; ADC tokens are short-lived (~1h) and this is what makes a
/// long-lived process keep working.
pub struct GoogleAdc {
    scope: String,
    // A `Mutex`, not a `RefCell`: `Credential` is `Sync`, so `token()` can be
    // called from several threads at once and interior mutability has to be
    // thread-safe. At worst two threads mint a token concurrently and the
    // later write wins — both are valid, so the lock is never held across the
    // network call.
    cached: Mutex<Option<(String, Instant)>>,
}

impl GoogleAdc {
    /// The cloud-platform scope covers Vertex AI.
    pub fn new() -> Self {
        Self::with_scope("https://www.googleapis.com/auth/cloud-platform")
    }

    pub fn with_scope(scope: impl Into<String>) -> Self {
        GoogleAdc {
            scope: scope.into(),
            cached: Mutex::new(None),
        }
    }

    fn metadata_host() -> String {
        std::env::var("GCE_METADATA_HOST").unwrap_or_else(|_| "169.254.169.254".to_string())
    }

    /// The metadata server gets its OWN agent, not the shared LLM one.
    ///
    /// `crate::agent()` is tuned for a model call — 30s to connect, 120s to
    /// answer — which is exactly wrong here. `169.254.169.254` is a
    /// link-local address that, off GCP, usually blackholes rather than
    /// refusing: the connect does not fail, it hangs. With the shared agent
    /// every uncached `token()` on a developer machine or a non-GCP host paid
    /// 30s before reaching the ADC file that was going to answer anyway, and
    /// paid it again on the next call because nothing is cached on failure.
    ///
    /// On a real GCP workload this endpoint answers in single-digit
    /// milliseconds over the loopback-ish link, so a 1s connect / 3s response
    /// budget is generous there and near-instant everywhere else. A miss here
    /// is not an error — it is "not on GCP", and the file path below is the
    /// answer.
    fn metadata_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(1)))
            .timeout_recv_response(Some(Duration::from_secs(3)))
            .timeout_recv_body(Some(Duration::from_secs(3)))
            .build()
            .into()
    }

    /// Ask the instance metadata server for the attached service account's
    /// token. Short timeout (see [`metadata_agent`](Self::metadata_agent)):
    /// off-GCP this endpoint is a black hole, and the file path below is the
    /// real fallback, not an error.
    fn fetch_from_metadata_server(&self) -> Result<(String, u64)> {
        let url = format!(
            "http://{}/computeMetadata/v1/instance/service-accounts/default/token?scopes={}",
            Self::metadata_host(),
            urlencode(&self.scope)
        );
        let text = Self::metadata_agent()
            .get(&url)
            .header("Metadata-Flavor", "Google")
            .call()
            .map_err(|e| Error::LlmBackend(format!("metadata server: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::LlmBackend(format!("metadata server: {e}")))?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Error::LlmBackend(format!("metadata server response: {e}")))?;
        let tok = v
            .get("access_token")
            .and_then(|s| s.as_str())
            .ok_or_else(|| Error::LlmBackend("metadata server returned no access_token".into()))?;
        let ttl = v.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(3600);
        Ok((tok.to_string(), ttl))
    }

    fn adc_file_path() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
            return Some(std::path::PathBuf::from(p));
        }
        let home = std::env::var("HOME").ok()?;
        let p = std::path::Path::new(&home)
            .join(".config/gcloud/application_default_credentials.json");
        p.exists().then_some(p)
    }

    /// Exchange a gcloud `authorized_user` refresh token for an access token.
    fn fetch_from_adc_file(&self) -> Result<(String, u64)> {
        let path = Self::adc_file_path()
            .ok_or_else(|| Error::LlmBackend("no ADC file found".into()))?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::LlmBackend(format!("{}: {e}", path.display())))?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| Error::LlmBackend(format!("{}: {e}", path.display())))?;
        match v.get("type").and_then(|s| s.as_str()) {
            Some("authorized_user") => {}
            Some("service_account") => {
                return Err(Error::LlmBackend(format!(
                    "{} is a service-account KEY file, which this backend deliberately does \
                     not support (signing its JWT needs an RSA implementation this tree does \
                     not carry, and the deployments ADC exists for typically forbid creating \
                     such keys). Use workload identity — an attached service account, reached \
                     through the metadata server — or `gcloud auth application-default login`.",
                    path.display()
                )))
            }
            other => {
                return Err(Error::LlmBackend(format!(
                    "{}: unsupported ADC credential type {other:?}",
                    path.display()
                )))
            }
        }
        let get = |k: &str| {
            v.get(k)
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .ok_or_else(|| Error::LlmBackend(format!("{}: missing {k}", path.display())))
        };
        let body = format!(
            "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
            urlencode(&get("client_id")?),
            urlencode(&get("client_secret")?),
            urlencode(&get("refresh_token")?),
        );
        let text = crate::agent()
            .post("https://oauth2.googleapis.com/token")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(&body)
            .map_err(|e| Error::LlmBackend(format!("oauth2 token exchange: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::LlmBackend(format!("oauth2 token exchange: {e}")))?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Error::LlmBackend(format!("oauth2 response: {e}")))?;
        let tok = v
            .get("access_token")
            .and_then(|s| s.as_str())
            .ok_or_else(|| Error::LlmBackend(format!("oauth2 returned no access_token: {text}")))?;
        let ttl = v.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(3600);
        Ok((tok.to_string(), ttl))
    }
}

impl Default for GoogleAdc {
    fn default() -> Self {
        Self::new()
    }
}

impl Credential for GoogleAdc {
    fn token(&self) -> Result<String> {
        if let Ok(guard) = self.cached.lock() {
            if let Some((tok, exp)) = guard.as_ref() {
                if Instant::now() < *exp {
                    return Ok(tok.clone());
                }
            }
        }
        // Metadata server first: on a GCP workload it is authoritative and the
        // file usually is not there at all.
        let (tok, ttl) = match self.fetch_from_metadata_server() {
            Ok(v) => v,
            Err(meta_err) => self.fetch_from_adc_file().map_err(|file_err| {
                Error::LlmBackend(format!(
                    "no Application Default Credentials: metadata server ({meta_err}); \
                     ADC file ({file_err})"
                ))
            })?,
        };
        // Refresh a minute early so a token never expires mid-request.
        let ttl = Duration::from_secs(ttl.saturating_sub(60).max(30));
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some((tok.clone(), Instant::now() + ttl));
        }
        Ok(tok)
    }

    fn kind(&self) -> &'static str {
        "google-adc"
    }
}

/// Minimal percent-encoding for the few values that reach a URL or form body
/// here (OAuth scopes, client ids, refresh tokens). Hand-rolled rather than
/// pulling a crate for it, matching the tree's dependency posture.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_key_round_trips() {
        let c = StaticKey::new("sk-test");
        assert_eq!(c.token().unwrap(), "sk-test");
        assert_eq!(c.kind(), "static");
    }

    #[test]
    fn urlencode_escapes_what_a_scope_and_a_token_contain() {
        assert_eq!(
            urlencode("https://www.googleapis.com/auth/cloud-platform"),
            "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform"
        );
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(urlencode("1//abc+def=="), "1%2F%2Fabc%2Bdef%3D%3D");
    }

    /// A service-account key file must be refused BY NAME, not reported as
    /// "no credentials": the operator needs to know the file was found and
    /// deliberately not used.
    #[test]
    fn service_account_key_files_are_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sa.json");
        std::fs::write(&p, r#"{"type":"service_account","project_id":"x"}"#).unwrap();
        // SAFETY: single-threaded test process; the var is restored below.
        unsafe { std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &p) };
        let err = GoogleAdc::new().fetch_from_adc_file().unwrap_err().to_string();
        unsafe { std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS") };
        assert!(err.contains("service-account KEY file"), "got: {err}");
        assert!(err.contains("workload identity"), "must name the fix: {err}");
    }
}
