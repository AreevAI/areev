//! The three imports, the two exports, and the allocator behind them.
//!
//! The sandbox freezes the import set (`areev::emit`, plus `areev::fetch` and
//! `areev::blob_get` when the host linked them) and calls into two exports:
//! `alloc(len) -> ptr` to place the input, then `run(ptr, len)`. A guest that
//! never calls `emit` is a trap, not an empty result — so every path here ends
//! in one.

use alloc::alloc::{GlobalAlloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;

// The host functions. Declared in one place so a tool's own source says
// nothing about the ABI beyond which capability it uses.
//
// An import appears in the built module only when it is *called*: `emit` in
// all of them, `fetch` in the three that talk HTTP, `blob_get` only in the one
// that reads a stored attachment. That is load-bearing, not incidental — the
// sandbox refuses an import the host did not link, by name, before one
// instruction runs, so a module that imported all three would be refused
// everywhere but the most permissive host. `build.sh` asserts the import set
// of every built blob for exactly that reason.
#[link(wasm_import_module = "areev")]
extern "C" {
    fn emit(ptr: i32, len: i32);
    fn fetch(ptr: i32, len: i32) -> i32;
    fn blob_get(ptr: i32, len: i32) -> i32;
}

const PAGE: usize = 65536;

/// Next free byte, and the end of what the memory currently holds. Zero means
/// "not started yet" — the first allocation reads the memory's current size, so
/// the heap begins above whatever the linker placed and the shadow stack uses.
static mut BUMP: usize = 0;
static mut END: usize = 0;

/// A bump allocator that never frees.
///
/// One input, one shaping pass, one result, then the process ends — a freeing
/// allocator would be dead weight and a source of nondeterminism in a module
/// whose fuel use is part of its contract. Memory is bounded by the sandbox's
/// page ceiling, so the failure mode of a runaway allocation is a refused grow
/// (a trap), not a host under pressure.
pub struct Bump;

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { bump(layout.size(), layout.align()) }
    }
    /// Deliberately nothing. See the type's doc.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

/// # Safety
/// Single-threaded by construction: wasm32 core modules have no threads here,
/// and the sandbox calls `alloc`/`run` from one host thread.
unsafe fn bump(size: usize, align: usize) -> *mut u8 {
    unsafe {
        if END == 0 {
            let size_pages = core::arch::wasm32::memory_size(0);
            BUMP = size_pages * PAGE;
            END = BUMP;
        }
        let align = if align == 0 { 1 } else { align };
        let start = (BUMP + align - 1) & !(align - 1);
        let want = start.saturating_add(size);
        if want > END {
            let pages = (want - END).div_ceil(PAGE);
            if core::arch::wasm32::memory_grow(0, pages) == usize::MAX {
                // Out of memory inside a sandbox with a declared ceiling: a
                // trap is the honest answer, and the host reports it as one.
                core::arch::wasm32::unreachable()
            }
            END += pages * PAGE;
        }
        BUMP = want;
        start as *mut u8
    }
}

/// The host's `alloc` export: it places the input here, and places a `fetch`
/// or `blob_get` reply here too.
pub fn guest_alloc(n: i32) -> i32 {
    let n = if n < 0 { 0 } else { n as usize };
    unsafe { bump(n.max(1), 8) as i32 }
}

/// Read the input the host placed at `ptr`.
///
/// # Safety
/// Called only from a `run(ptr, len)` export with the host's own arguments.
pub unsafe fn input(ptr: i32, len: i32) -> &'static [u8] {
    if ptr <= 0 || len <= 0 {
        return &[];
    }
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

/// Hand a result back and return. Every exit from a tool goes through here.
pub fn emit_bytes(bytes: &[u8]) {
    unsafe { emit(bytes.as_ptr() as i32, bytes.len() as i32) }
}

pub fn emit_str(s: &str) {
    emit_bytes(s.as_bytes());
}

/// A host reply: `[u32 little-endian length][bytes]` at the returned pointer,
/// allocated through our own `alloc`. A negative return means the host could
/// not place a response at all — every other outcome, refusal included,
/// arrives as ordinary bytes.
fn reply(ret: i32) -> Option<Vec<u8>> {
    if ret < 0 {
        return None;
    }
    let p = ret as usize;
    let len = unsafe {
        let hdr = core::slice::from_raw_parts(p as *const u8, 4);
        u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize
    };
    let body = unsafe { core::slice::from_raw_parts((p + 4) as *const u8, len) };
    Some(body.to_vec())
}

/// One brokered HTTP call. `request` is the broker's own request shape,
/// forwarded rather than translated — there is no place here for a
/// translation bug.
pub fn brokered_fetch(request: &[u8]) -> Result<Vec<u8>, &'static str> {
    let ret = unsafe { fetch(request.as_ptr() as i32, request.len() as i32) };
    reply(ret).ok_or("the host could not place a response for areev::fetch")
}

/// One CAS blob read, by content address. Returns the blob's own bytes.
pub fn brokered_blob(request: &[u8]) -> Result<Vec<u8>, &'static str> {
    let ret = unsafe { blob_get(request.as_ptr() as i32, request.len() as i32) };
    reply(ret).ok_or("the host could not place a response for areev::blob_get")
}

/// `{"error": "..."}` — the shape a tool fails in, so a caller has one thing
/// to check whatever went wrong.
pub fn error_json(detail: &str) -> String {
    let mut s = String::from("{\"error\":\"");
    crate::json::escape_into(detail.as_bytes(), &mut s);
    s.push_str("\"}");
    s
}

/// Declare the guest half: the allocator, the panic handler, and the `alloc`
/// export the host calls to place input.
///
/// A macro rather than plain items because `#[panic_handler]` and
/// `#[global_allocator]` belong to the final artifact: each tool is its own
/// `cdylib`, and each must carry exactly one.
#[macro_export]
macro_rules! guest_abi {
    () => {
        #[global_allocator]
        static AREEV_ALLOC: $crate::abi::Bump = $crate::abi::Bump;

        /// A trap, not a message: there is no stderr here, and the sandbox
        /// reports a trap with the fuel it cost. Panics are bugs in a blessed
        /// blob, and every reachable failure returns `{"error": ...}` instead.
        #[panic_handler]
        fn areev_panic(_info: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }

        #[no_mangle]
        pub extern "C" fn alloc(n: i32) -> i32 {
            $crate::abi::guest_alloc(n)
        }
    };
}
