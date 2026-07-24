//! ABI v1 — the wire protocol between host and guest.
//!
//! Payloads are UTF-8 JSON in the guest's linear memory, addressed as a
//! pointer/length pair packed into one `u64`: `(ptr << 32) | len`. A guest
//! module implements five exports and may import two host functions from the
//! `"tacitus"` module — that import surface is the entire sandbox boundary.
//!
//! Guest exports:
//! - `memory` — the linear memory
//! - `tacitus_abi_version() -> i32` — must return [`ABI_VERSION`]
//! - `tacitus_alloc(len: i32) -> i32` — pointer to `len` writable bytes
//! - `tacitus_dealloc(ptr: i32, len: i32)` — free (a no-op is fine)
//! - `tacitus_run(ptr: i32, len: i32) -> i64` — entry point; input JSON at
//!   ptr/len, returns a packed ptr/len of a `{ ok, data | error }` envelope
//!
//! Host imports (module `"tacitus"`):
//! - `call(tool_ptr, tool_len, args_ptr, args_len) -> i64` — run a tool from
//!   the manifest allowlist; ALWAYS returns an envelope (denial is
//!   `PERMISSION_DENIED` data, never a trap)
//! - `log(level, ptr, len)` — 0=debug 1=info 2=warn 3=error, captured by host

pub const ABI_VERSION: i32 = 1;

pub const IMPORT_MODULE: &str = "tacitus";
pub const IMPORT_CALL: &str = "call";
pub const IMPORT_LOG: &str = "log";

pub const EXPORT_MEMORY: &str = "memory";
pub const EXPORT_ABI_VERSION: &str = "tacitus_abi_version";
pub const EXPORT_ALLOC: &str = "tacitus_alloc";
pub const EXPORT_DEALLOC: &str = "tacitus_dealloc";
pub const EXPORT_RUN: &str = "tacitus_run";

/// Pack a guest pointer + length into the single `u64` the ABI passes around.
pub fn pack(ptr: u32, len: u32) -> u64 {
    (u64::from(ptr) << 32) | u64::from(len)
}

/// Inverse of [`pack`].
pub fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        for (ptr, len) in [(0u32, 0u32), (1024, 17), (u32::MAX, u32::MAX)] {
            assert_eq!(unpack(pack(ptr, len)), (ptr, len));
        }
    }
}
