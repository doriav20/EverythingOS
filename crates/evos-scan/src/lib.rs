//! Filesystem scanning for EverythingOS: volume discovery, the parallel
//! walker, live change watchers for local filesystems and the adaptive
//! scheduler that keeps network shares fresh.

pub mod changes;
pub mod fanotify;
pub mod fsops;
pub mod inotify;
pub mod mountinfo;
pub mod sweep;
pub mod throttle;
pub mod walker;
