#![no_main]
use libfuzzer_sys::fuzz_target;

use areev_core::format::tool_schema::ProviderKind;

// Provider tool-call extraction parses untrusted model output with (per the
// source) "ReDoS-guarded regexes". Fuzz every provider to prove the guards
// hold and nothing panics on adversarial text.
fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let providers = [
        ProviderKind::Hermes,
        ProviderKind::Llama31,
        ProviderKind::AnthropicTools,
        ProviderKind::OpenAiTools,
        ProviderKind::OpenAiResponses,
        ProviderKind::MarkdownTools,
    ];
    // First byte selects the provider so the fuzzer can explore all branches.
    let provider = providers[data[0] as usize % providers.len()];
    if let Ok(s) = std::str::from_utf8(&data[1..]) {
        let _ = areev_core::format::tool_schema::parse::parse(provider, s);
    }
});
