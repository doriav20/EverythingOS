//! The EverythingOS daemon: owns the volume indexes, keeps them current,
//! manages mounts and answers clients over a Unix socket.

pub mod config;
pub mod mounts;
pub mod server;
pub mod volumes;
