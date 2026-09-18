//! Per-endpoint request shaping (#285).
//!
//! The two built-in adapters used to send a fixed body: `max_tokens` and
//! `temperature`, nothing else, no way to add a field. That is not a small
//! gap. OpenAI's reasoning models take `max_completion_tokens` rather than
//! `max_tokens` and several reject a non-default `temperature`; OpenRouter
//! needs `{"provider": {...}}` to pin which upstream (and which jurisdiction)
//! actually serves a request; the Anthropic wire format takes `thinking`,
//! `output_config` and `inference_geo`. None of it was reachable, so Areev's
//! own benchmarks pin OpenRouter providers from Python instead of through the
//! engine.
//!
//! The shape here is deliberate. Areev does NOT track vendor parameter lists
//! — that is the stale-table problem `context_window()` is documented as
//! avoiding. It owns a handful of fields and passes everything else through
//! untouched, refusing at CONSTRUCTION (not at call time) any key that would
//! collide with one it owns, so a misconfiguration is a startup error rather
//! than a failed run.

use serde_json::{Map, Value};

use areev_loop::{Error, Result};

/// Which field carries the output-token ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenLimitField {
    /// `max_tokens` — the default, and what vLLM, llama.cpp, LM Studio and
    /// Ollama's compatibility endpoint all take. A blanket swap is NOT the
    /// fix here: those are named targets.
    #[default]
    MaxTokens,
    /// `max_completion_tokens` — OpenAI's reasoning models.
    MaxCompletionTokens,
}

/// Whether to send a sampling temperature at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TemperatureMode {
    #[default]
    Send,
    Omit,
}

/// How one endpoint wants its request body shaped.
///
/// [`RequestProfile::default()`] produces byte-for-byte today's request, so
/// installing a default profile changes nothing.
#[derive(Debug, Clone, Default)]
pub struct RequestProfile {
    pub token_limit: TokenLimitField,
    pub temperature: TemperatureMode,
    extra_body: Map<String, Value>,
}

/// Keys the adapters own. `extra_body` may not set any of them: a profile
/// that could overwrite `model` or `messages` would let configuration
/// silently redirect a governed run to another model, and a profile that
/// could set `max_tokens` would fight `token_limit` on the same body.
pub const RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
];

impl RequestProfile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn token_limit(mut self, f: TokenLimitField) -> Self {
        self.token_limit = f;
        self
    }

    pub fn temperature(mut self, m: TemperatureMode) -> Self {
        self.temperature = m;
        self
    }

    /// Extra top-level request fields, merged last and passed through
    /// untouched — `store`, `reasoning_effort`, `provider`, `thinking`,
    /// `output_config`, `inference_geo`, `seed`, anything a vendor adds
    /// tomorrow.
    ///
    /// Refused at construction when it names a key the adapter owns, naming
    /// the key: a run that starts is a run an operator believes is
    /// configured as asked.
    pub fn extra_body(mut self, extra: Map<String, Value>) -> Result<Self> {
        for k in extra.keys() {
            if RESERVED_BODY_KEYS.contains(&k.as_str()) {
                return Err(Error::LlmBackend(format!(
                    "request profile may not set {k:?} — the adapter owns it \
                     (reserved: {})",
                    RESERVED_BODY_KEYS.join(", ")
                )));
            }
        }
        self.extra_body = extra;
        Ok(self)
    }

    /// Parse `extra_body` from a JSON object string, as a CLI flag supplies it.
    pub fn extra_body_json(self, json: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(json)
            .map_err(|e| Error::LlmBackend(format!("--llm-extra-body is not valid JSON: {e}")))?;
        match v {
            Value::Object(map) => self.extra_body(map),
            _ => Err(Error::LlmBackend(
                "--llm-extra-body must be a JSON object".into(),
            )),
        }
    }

    /// The token-limit field name for this profile.
    pub fn token_limit_key(&self) -> &'static str {
        match self.token_limit {
            TokenLimitField::MaxTokens => "max_tokens",
            TokenLimitField::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    pub fn sends_temperature(&self) -> bool {
        matches!(self.temperature, TemperatureMode::Send)
    }

    /// Merge the extra fields into a built body. Called LAST, but it can
    /// never clobber an owned key — the constructor already refused those.
    pub fn apply_extra(&self, body: &mut Value) {
        let Some(obj) = body.as_object_mut() else { return };
        for (k, v) in &self.extra_body {
            obj.insert(k.clone(), v.clone());
        }
    }

    /// Whether this profile is the default one (used to keep the run
    /// manifest's LLM pin absent when nothing was configured).
    pub fn is_default(&self) -> bool {
        self.token_limit == TokenLimitField::default()
            && self.temperature == TemperatureMode::default()
            && self.extra_body.is_empty()
    }

    /// A stable content address of this profile, for the run manifest's LLM
    /// pin (#287): what was sent is part of what a run ran under.
    pub fn digest(&self) -> Option<String> {
        if self.is_default() {
            return None;
        }
        // `Map` is a BTreeMap under serde_json's default features, so the
        // serialization is key-sorted and the digest is stable.
        let canonical = serde_json::json!({
            "token_limit": self.token_limit_key(),
            "temperature": if self.sends_temperature() { "send" } else { "omit" },
            "extra_body": Value::Object(self.extra_body.clone()),
        });
        let digest = areev_core::anon::hmac_sha256(
            b"areev-request-profile/v1",
            canonical.to_string().as_bytes(),
        );
        Some(digest.iter().map(|b| format!("{b:02x}")).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_profile_changes_nothing() {
        let p = RequestProfile::default();
        assert_eq!(p.token_limit_key(), "max_tokens");
        assert!(p.sends_temperature());
        assert!(p.is_default());
        assert!(p.digest().is_none(), "a default profile pins nothing");
        let mut body = serde_json::json!({"model": "m"});
        p.apply_extra(&mut body);
        assert_eq!(body, serde_json::json!({"model": "m"}));
    }

    #[test]
    fn every_owned_key_is_refused_by_name() {
        for k in RESERVED_BODY_KEYS {
            let mut m = Map::new();
            m.insert((*k).to_string(), Value::from("x"));
            let err = RequestProfile::new().extra_body(m).unwrap_err();
            assert!(err.to_string().contains(k), "{k}: {err}");
        }
    }

    #[test]
    fn unknown_keys_pass_through_verbatim() {
        let p = RequestProfile::new()
            .extra_body_json(r#"{"store":false,"reasoning_effort":"low"}"#)
            .unwrap();
        let mut body = serde_json::json!({"model": "m"});
        p.apply_extra(&mut body);
        assert_eq!(body["store"], serde_json::json!(false));
        assert_eq!(body["reasoning_effort"], serde_json::json!("low"));
    }

    #[test]
    fn a_nested_vendor_object_survives_intact() {
        let p = RequestProfile::new()
            .extra_body_json(
                r#"{"provider":{"only":["x"],"allow_fallbacks":false,"data_collection":"deny"}}"#,
            )
            .unwrap();
        let mut body = serde_json::json!({});
        p.apply_extra(&mut body);
        assert_eq!(body["provider"]["only"], serde_json::json!(["x"]));
        assert_eq!(body["provider"]["allow_fallbacks"], serde_json::json!(false));
        assert_eq!(body["provider"]["data_collection"], serde_json::json!("deny"));
    }

    #[test]
    fn malformed_json_is_refused_before_anything_runs() {
        assert!(RequestProfile::new().extra_body_json("{nope").is_err());
        assert!(RequestProfile::new().extra_body_json("[1,2]").is_err());
    }

    #[test]
    fn the_digest_is_stable_and_distinguishes_profiles() {
        let a = RequestProfile::new().extra_body_json(r#"{"a":1,"b":2}"#).unwrap();
        let b = RequestProfile::new().extra_body_json(r#"{"b":2,"a":1}"#).unwrap();
        assert_eq!(a.digest(), b.digest(), "key order must not change the pin");
        let c = RequestProfile::new().temperature(TemperatureMode::Omit);
        assert_ne!(a.digest(), c.digest());
    }
}
