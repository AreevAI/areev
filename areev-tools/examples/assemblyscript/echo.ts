// echo-as — a wasm32-areev reference module in AssemblyScript (#340).
//
// Emits its input as its output, byte for byte. The contract is
// docs/sandbox-abi.md; this file is that contract and nothing more:
//
//   import  areev::emit(ptr, len)   hand the result back
//   export  alloc(len) -> ptr       the host places the input here
//   export  run(ptr, len)           and calls this
//   export  memory                  with a declared maximum (--maximumMemory)
//
// Built with `--runtime stub` (a bump allocator, no garbage collector to
// export) and `--use abort=` (no `env::abort` import, which the sandbox would
// refuse by name). Nothing here reads a clock or `Math.random`, both of which
// would add an `env` import and be refused the same way.

// The one import. `@external` sets the import module and name; the default
// module ("env") would be refused at instantiation.
@external("areev", "emit")
declare function emit(ptr: usize, len: i32): void;

// The stub runtime's heap: a bump allocator that grows memory on demand and
// traps at the declared maximum rather than handing back a bad pointer.
export function alloc(len: i32): usize {
  return heap.alloc(<usize>len);
}

export function run(ptr: usize, len: i32): void {
  // UTF-8 JSON in, so UTF-8 JSON out: echo needs no parser to keep that.
  emit(ptr, len);
}
