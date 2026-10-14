//! Authorization of privileged requests through polkit.
//!
//! The daemon runs as root and mounts on behalf of users. Root callers are
//! always allowed; everyone else is checked against a polkit action, which
//! by default asks an administrator password through the desktop's polkit
//! agent and remembers it for a few minutes.

use std::collections::HashMap;

use zbus::zvariant::Value;

/// polkit action for adding, changing and removing mounts.
pub const MANAGE_MOUNTS: &str = "org.evos.manage-mounts";

/// `AllowUserInteraction`: let the caller's polkit agent prompt.
const ALLOW_INTERACTION: u32 = 1;

/// Start time of a process in clock ticks since boot, which polkit uses
/// together with the pid to rule out pid reuse.
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses; fields resume
    // after the last ')'. starttime is field 22, the 20th after it.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Asks polkit whether process `pid` (running as `uid`) may perform
/// `action`, letting its agent prompt for credentials.
pub async fn polkit_authorized(pid: u32, uid: u32, action: &str) -> Result<bool, String> {
    let start_time = process_start_time(pid).ok_or("cannot identify the calling process")?;
    let conn = zbus::Connection::system()
        .await
        .map_err(|e| format!("cannot reach the system bus: {e}"))?;
    let subject_details: HashMap<&str, Value<'_>> = HashMap::from([
        ("pid", Value::U32(pid)),
        ("start-time", Value::U64(start_time)),
        ("uid", Value::I32(uid as i32)),
    ]);
    let details: HashMap<&str, &str> = HashMap::new();
    let reply = conn
        .call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            &(
                ("unix-process", subject_details),
                action,
                details,
                ALLOW_INTERACTION,
                "",
            ),
        )
        .await
        .map_err(|e| format!("polkit: {e}"))?;
    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = reply
        .body()
        .deserialize()
        .map_err(|e| format!("polkit reply: {e}"))?;
    Ok(authorized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_start_time() {
        let t = process_start_time(std::process::id()).unwrap();
        assert!(t > 0);
        assert!(process_start_time(u32::MAX).is_none());
    }
}
