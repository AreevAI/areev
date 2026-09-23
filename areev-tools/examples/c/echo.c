/*
 * echo-c — a wasm32-areev reference module in freestanding C (#340).
 *
 * Emits its input as its output, byte for byte. It exists to show the whole
 * guest contract in one screen of a language that is not Rust; the contract
 * itself is docs/sandbox-abi.md, and nothing here goes beyond it:
 *
 *   import  areev::emit(ptr, len)   hand the result back
 *   export  alloc(len) -> ptr       the host places the input here
 *   export  run(ptr, len)           and calls this
 *   export  memory                  with a declared maximum (the linker's
 *                                   --max-memory; see build.sh)
 *
 * No libc (-nostdlib): there is no clock, no randomness, no environment and
 * no file to reach, so there is nothing a libc could usefully provide — and
 * wasi-libc would import wasi_snapshot_preview1, which the sandbox refuses.
 */

/* The one import. `import_module`/`import_name` put it under "areev" instead
 * of clang's default "env", which the sandbox would refuse by name. */
__attribute__((import_module("areev"), import_name("emit")))
extern void areev_emit(const unsigned char *ptr, int len);

/* A bump allocator over linear memory, growing it on demand. Nothing is ever
 * freed: one process runs one call, and the memory goes with the instance. */
static unsigned long heap_top;

__attribute__((export_name("alloc")))
unsigned char *alloc(int len) {
    if (len < 0) __builtin_trap();
    if (heap_top == 0) {
        /* Start past everything the linker laid out (data + stack). */
        heap_top = __builtin_wasm_memory_size(0) * 65536UL;
    }
    unsigned long start = (heap_top + 7UL) & ~7UL;
    unsigned long end = start + (unsigned long)len;
    unsigned long have = __builtin_wasm_memory_size(0) * 65536UL;
    if (end > have) {
        unsigned long pages = (end - have + 65535UL) / 65536UL;
        /* -1 = the declared maximum would be exceeded. Trap rather than hand
         * back a pointer the host would write through. */
        if (__builtin_wasm_memory_grow(0, pages) == (unsigned long)-1) __builtin_trap();
    }
    heap_top = end;
    return (unsigned char *)start;
}

__attribute__((export_name("run")))
void run(const unsigned char *ptr, int len) {
    /* The input is UTF-8 JSON, and so the output is too: echo is the one
     * tool that needs no parser to keep that promise. */
    areev_emit(ptr, len);
}
