//! Shared escape helpers for text-format tool adapters (SR-F1).
//!
//! Every text-format adapter must sanitize user-controlled strings
//! (`tool_description`, schema `enum`/`default`/`examples`) before
//! embedding them in its envelope. Without this, a malicious or
//! inattentive tool author could inject `</tool>`, `<|python_tag|>`,
//! triple-backticks, or similar tokens that break the downstream LLM's
//! parsing of the tool catalog.

/// XML/SML entity escape — mirrors `context::render::sml_escape` but
/// lives here to keep `format::tool_schema` self-contained. Used by
/// `sml.rs`.
pub fn sml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Hermes adapter escape: strip literal `</tool>` and `<tool>` close
/// tags from prose, and pick a fence that does not collide with the
/// content. Returns (escaped_description, fence).
pub fn hermes_sanitize(description: &str) -> (String, &'static str) {
    let mut cleaned = description
        .replace("</tool>", "&lt;/tool&gt;")
        .replace("<tool>", "&lt;tool&gt;")
        .replace("</parameters>", "&lt;/parameters&gt;");
    // Collapse any leading/trailing whitespace the replacements left.
    cleaned = cleaned.trim().to_string();
    let fence = if cleaned.contains("```") {
        "~~~"
    } else {
        "```"
    };
    (cleaned, fence)
}

/// Llama 3.1 adapter escape: strip `<|...|>` control-token runs that
/// the Llama tokenizer would misinterpret if they appeared inside a
/// tool description.
pub fn llama31_sanitize(description: &str) -> String {
    // Match any <|token|> sequence greedily and remove it.
    let mut out = String::with_capacity(description.len());
    let bytes = description.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && &bytes[i..=i + 1] == b"<|" {
            // Seek forward to |>
            let rest = &description[i..];
            if let Some(end) = rest.find("|>") {
                // Skip the whole <|...|> token.
                i += end + 2;
                continue;
            }
        }
        // Safe: i is on a char boundary because we only advance by
        // `end + 2` which was computed from a byte index into `rest`
        // whose start is a valid boundary. For the normal path we push
        // the next char by advancing by its UTF-8 length.
        let ch = description[i..].chars().next().expect("non-empty");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Markdown adapter: pick a fence token that doesn't collide with the
/// content. Returns `("```"/"~~~", description)` — description itself
/// needs no escaping beyond fence selection since it's prose.
pub fn markdown_fence(content: &str) -> &'static str {
    if content.contains("```") {
        "~~~"
    } else {
        "```"
    }
}

/// Pretty-print a JSON Value to a compact single-line string, stable
/// across calls (BTreeMap via serde_json's preserve_order=off default).
pub fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sml_escape_handles_angle_brackets() {
        assert_eq!(sml_escape("a<b"), "a&lt;b");
        assert_eq!(sml_escape("</tool>"), "&lt;/tool&gt;");
        assert_eq!(sml_escape("a & b"), "a &amp; b");
        assert_eq!(sml_escape("\"quoted\""), "&quot;quoted&quot;");
    }

    #[test]
    fn hermes_sanitize_strips_tool_tags() {
        let (cleaned, _) = hermes_sanitize("Post to </tool> and <tool>");
        assert!(!cleaned.contains("</tool>"));
        assert!(!cleaned.contains("<tool>"));
    }

    #[test]
    fn hermes_sanitize_picks_tilde_fence_on_collision() {
        let (_, fence) = hermes_sanitize("here is a ``` triple backtick");
        assert_eq!(fence, "~~~");
        let (_, fence) = hermes_sanitize("no collision");
        assert_eq!(fence, "```");
    }

    #[test]
    fn llama31_strips_pipe_tokens() {
        let cleaned = llama31_sanitize("Use <|python_tag|>print<|eom_id|> to run code");
        assert!(!cleaned.contains("<|"));
        assert!(!cleaned.contains("|>"));
        assert!(cleaned.contains("Use"));
        assert!(cleaned.contains("print"));
        assert!(cleaned.contains("to run code"));
    }

    #[test]
    fn llama31_preserves_unicode() {
        let cleaned = llama31_sanitize("description with emoji 🔥 and <|bad|>");
        assert!(cleaned.contains("🔥"));
        assert!(!cleaned.contains("<|bad|>"));
    }

    #[test]
    fn markdown_fence_avoids_collision() {
        assert_eq!(markdown_fence("simple prose"), "```");
        assert_eq!(markdown_fence("```inline fence```"), "~~~");
    }
}
