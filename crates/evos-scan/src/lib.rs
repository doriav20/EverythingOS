//! Filesystem scanning for EverythingOS: volume discovery, the parallel
//! walker, live change watchers for local filesystems and the adaptive
//! scheduler that keeps network shares fresh.

pub mod fsops;
pub mod mountinfo;
pub mod walker;
