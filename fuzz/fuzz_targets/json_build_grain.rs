#![no_main]
use libfuzzer_sys::fuzz_target;

// The JSON grain builder runs on untrusted field maps from every write
// surface (CAL ADD/SUPERSEDE, MCP areev_add, both bindings' add()). The
// Wave-0 rule is "never panics, never silently accepts": arbitrary JSON must
// come back as a built grain or a Validation error — a panic here is a DoS on
// every host, and the related_to / run_id / tool-vocabulary validators are
// exactly the code paths taking adversarial shapes.
//
// The sink serializes the accepted grain, so serialize invariants (a grain
// that can be written can be read) get fuzzed in the same pass.
struct SerializeSink;
impl areev_cal::json_build::GrainSink for SerializeSink {
    type Out = ();
    fn consume<G: areev_core::types::Grain + Clone + 'static>(
        self,
        grain: &G,
    ) -> areev_core::error::Result<Self::Out> {
        let _ = areev_core::format::serialize::serialize_grain(grain)?;
        Ok(())
    }
}

const TYPES: &[&str] = &[
    "fact", "event", "state", "workflow", "tool", "observation", "goal",
    "reasoning", "consensus", "consent", "skill",
];

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(s) else {
        return;
    };
    // First byte steers the grain type so the corpus explores every arm.
    let gt = TYPES[data.first().copied().unwrap_or(0) as usize % TYPES.len()];
    let _ = areev_cal::json_build::build_grain_from_json(gt, &fields, SerializeSink);
});
