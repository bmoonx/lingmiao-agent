//! Short record identifiers — mirrors the Python `uuid.uuid4().hex[:12]` form.
//!
//! The original generated ids like `obs-3f9a1c2b4d5e`. We keep the same
//! `{prefix}-{12 hex chars}` shape but derive them deterministically from the
//! wall clock plus a process-local counter, so no RNG dependency is needed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 64-bit FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// 64-bit FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Generate a short, unique-enough id with the given prefix.
///
/// The entropy mixes nanosecond time, a monotonically increasing counter and
/// the address of a stack local (ASLR), so ids stay distinct within a process
/// and across short-lived processes.
pub fn short_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stack = &nanos as *const u64 as u64;
    let mut seed = Vec::with_capacity(24);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&n.to_le_bytes());
    seed.extend_from_slice(&stack.to_le_bytes());
    let h = fnv1a(&seed);
    format!("{prefix}-{:012x}", h & 0x0000_ffff_ffff_ffff)
}

/// Current UTC timestamp in RFC-3339 (mirrors `datetime.now(timezone.utc).isoformat()`).
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_have_prefix_and_length() {
        let id = short_id("obs");
        assert!(id.starts_with("obs-"));
        assert_eq!(id.len(), 4 + 12);
    }

    #[test]
    fn ids_are_unique_within_process() {
        let ids: HashSet<String> = (0..2000).map(|_| short_id("node")).collect();
        assert_eq!(ids.len(), 2000);
    }

    #[test]
    fn now_iso_is_rfc3339() {
        let ts = now_iso();
        assert!(ts.contains('T') && (ts.ends_with('Z') || ts.contains('+')));
    }
}
