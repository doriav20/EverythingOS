//! Accounts of the network servers: the Linux user whose permissions
//! remote users get, and stored passwords.

use std::ffi::{CStr, CString};

use evos_core::perm::Caller;
use sha2::{Digest, Sha256};

/// The uid and groups of a Linux account, for filtering what remote users
/// may see and read. `None` if there is no such account.
pub fn caller_of(user: &str) -> Option<Caller> {
    let name = CString::new(user).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives pwd's use.
    let rc = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    let (uid, gid) = (pwd.pw_uid, pwd.pw_gid);
    let mut groups = vec![0 as libc::gid_t; 256];
    let mut n = groups.len() as libc::c_int;
    // SAFETY: groups has room for n entries; n is updated to the count.
    let rc = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
    let mut gids = vec![gid];
    if rc >= 0 {
        gids.extend(groups[..n.max(0) as usize].iter().copied());
    }
    gids.sort_unstable();
    gids.dedup();
    Some(Caller { uid, gids })
}

/// The name of the account with this uid, if any.
pub fn name_of(uid: u32) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: as in caller_of.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: pw_name points into buf and is NUL-terminated.
    let name = unsafe { CStr::from_ptr(pwd.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn random_salt() -> String {
    use std::io::Read;
    let mut salt = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut salt));
    if read.is_err() {
        // Fall back to the clock: still unique per password.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        salt.copy_from_slice(&now.to_le_bytes());
    }
    hex(&salt)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(salt: &str, password: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(b"\0");
    h.update(password.as_bytes());
    hex(&h.finalize())
}

/// A salted hash to store instead of a password.
pub fn hash_password(password: &str) -> String {
    let salt = random_salt();
    format!("sha256${salt}${}", digest(&salt, password))
}

/// Does `password` match a stored hash? An empty stored value means no
/// password is set (anything matches).
pub fn verify_password(stored: &str, password: &str) -> bool {
    if stored.is_empty() {
        return true;
    }
    let mut parts = stored.splitn(3, '$');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("sha256"), Some(salt), Some(hash)) => {
            let candidate = digest(salt, password);
            // Constant-time comparison.
            candidate.len() == hash.len()
                && candidate
                    .bytes()
                    .zip(hash.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords() {
        let stored = hash_password("שלום-123");
        assert!(stored.starts_with("sha256$"));
        assert!(verify_password(&stored, "שלום-123"));
        assert!(!verify_password(&stored, "wrong"));
        assert_ne!(hash_password("x"), hash_password("x"), "salted");
        assert!(verify_password("", "anything"), "no password set");
        assert!(!verify_password("garbage", "x"));
    }

    #[test]
    fn accounts() {
        let root = caller_of("root").expect("root exists");
        assert_eq!(root.uid, 0);
        assert!(root.gids.contains(&0));
        assert_eq!(name_of(0).as_deref(), Some("root"));
        assert!(caller_of("no-such-user-evos").is_none());
    }
}
