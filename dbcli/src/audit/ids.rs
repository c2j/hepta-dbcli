// ─── Audit identifiers (session_id / event_id) ──────────────────────
//
// Zero-dependency ids. `event_id` follows the ULID layout (48-bit ms
// timestamp + 80 bits of entropy, Crockford base32, 26 chars) so logs sort
// lexicographically by time. Entropy is derived in-process, not from a CSPRNG:
// ids are correlation handles, never security tokens (issue #57 D7).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::sha256::sha256;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn entropy() -> [u8; 32] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut seed = Vec::with_capacity(32);
    seed.extend_from_slice(b"hepta-dbcli-audit-v1");
    seed.extend_from_slice(&std::process::id().to_be_bytes());
    seed.extend_from_slice(&nanos.to_be_bytes());
    seed.extend_from_slice(&counter.to_be_bytes());
    sha256(&seed)
}

fn hex16(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 16 lowercase hex chars, constant for one process lifetime.
pub(crate) fn new_session_id() -> String {
    hex16(&entropy()[..8])
}

/// 26-char Crockford base32 ULID (time-ordered).
pub(crate) fn new_event_id() -> String {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let entropy = entropy();

    let mut bytes = [0u8; 16];
    let ts = ts_ms & 0x0000_FFFF_FFFF_FFFF;
    bytes[0] = ((ts >> 40) & 0xff) as u8;
    bytes[1] = ((ts >> 32) & 0xff) as u8;
    bytes[2] = ((ts >> 24) & 0xff) as u8;
    bytes[3] = ((ts >> 16) & 0xff) as u8;
    bytes[4] = ((ts >> 8) & 0xff) as u8;
    bytes[5] = (ts & 0xff) as u8;
    bytes[6..16].copy_from_slice(&entropy[..10]);

    let mut n = u128::from_be_bytes(bytes);
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(n & 0x1f) as usize];
        n >>= 5;
    }
    String::from_utf8(out.to_vec()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn should_produce_16_hex_chars_for_session_id() {
        let id = new_session_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(id.chars().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn should_produce_valid_ulid_shape() {
        let id = new_event_id();
        assert_eq!(id.len(), 26);
        assert!(
            id.bytes().all(|b| ALPHABET.contains(&b)),
            "non-alphabet char in {id}"
        );
        // 128 bits over 26 base32 chars leaves the top 2 bits zero.
        assert!(id.as_bytes()[0] <= b'7', "ulid overflow char in {id}");
    }

    #[test]
    fn should_not_collide_across_many_calls() {
        let ids: HashSet<String> = (0..1000).map(|_| new_event_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn should_sort_by_time_for_sequential_calls() {
        let a = new_event_id();
        let b = new_event_id();
        // Same millisecond is possible; the 80 random bits break ties, so the
        // only guarantee is that the timestamp prefix is non-decreasing.
        assert!(a[..10] <= b[..10]);
    }
}
