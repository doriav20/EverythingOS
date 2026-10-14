//! Mount management for EverythingOS: mount definitions, credential storage
//! and the mount/unmount/reconnect logic run by the daemon.

pub mod command;
pub mod store;

pub use store::MountStore;
