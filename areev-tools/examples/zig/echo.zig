//! echo-zig — a wasm32-areev reference module in Zig (#340).
//!
//! Emits its input as its output, byte for byte. The contract is
//! docs/sandbox-abi.md; this file is that contract and nothing more:
//!
//!   import  areev::emit(ptr, len)   hand the result back
//!   export  alloc(len) -> ptr       the host places the input here
//!   export  run(ptr, len)           and calls this
//!   export  memory                  with a declared maximum (--max-memory)
//!
//! `wasm32-freestanding` has no OS layer, so nothing here can reach a clock, a
//! random source or the environment — and nothing can import
//! wasi_snapshot_preview1, which the sandbox refuses.

/// The one import. `extern "areev"` sets the import MODULE; the default
/// ("env") would be refused by name at instantiation.
extern "areev" fn emit(ptr: [*]const u8, len: i32) void;

const page: usize = 65536;

/// Bump allocator over linear memory, growing it on demand. Nothing is freed:
/// one process runs one call, and the memory goes with the instance.
var heap_top: usize = 0;

export fn alloc(len: i32) [*]u8 {
    if (len < 0) @trap();
    if (heap_top == 0) {
        // Start past everything the linker laid out (data + stack).
        heap_top = @wasmMemorySize(0) * page;
    }
    const start = (heap_top + 7) & ~@as(usize, 7);
    const end = start + @as(usize, @intCast(len));
    const have = @wasmMemorySize(0) * page;
    if (end > have) {
        const pages = (end - have + page - 1) / page;
        // -1 = the declared maximum would be exceeded. Trap rather than hand
        // back a pointer the host would write through.
        if (@wasmMemoryGrow(0, pages) == -1) @trap();
    }
    heap_top = end;
    return @ptrFromInt(start);
}

export fn run(ptr: [*]const u8, len: i32) void {
    // UTF-8 JSON in, so UTF-8 JSON out: echo needs no parser to keep that.
    emit(ptr, len);
}
