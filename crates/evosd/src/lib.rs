//! The EverythingOS daemon: owns the volume indexes, keeps them current,
//! manages mounts and answers clients over a Unix socket.

pub mod accounts;
pub mod auth;
pub mod config;
pub mod connect;
pub mod daemon;
pub mod history;
pub mod mounts;
pub mod remote;
pub mod server;
pub mod servers;
pub mod volumes;
