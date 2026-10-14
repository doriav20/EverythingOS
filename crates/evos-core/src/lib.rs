//! Core of EverythingOS: the in-memory file index, the Everything-style query
//! language and the parallel search engine. This crate does no I/O besides
//! reading and writing index snapshots.

pub mod fold;
