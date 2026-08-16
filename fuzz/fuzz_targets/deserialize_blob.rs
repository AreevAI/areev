#![no_main]
use libfuzzer_sys::fuzz_target;

// The `.mg` blob deserializer is the primary untrusted-input surface: bundles
// and synced segments arrive as raw bytes from potentially hostile peers. It
// must NEVER panic, overflow the stack, or over-allocate — only return Ok/Err.
// (See `guard_msgpack_shape` in areev-core, which this exercises.)
fuzz_target!(|data: &[u8]| {
    let _ = areev_core::deserialize_blob(data);
});
