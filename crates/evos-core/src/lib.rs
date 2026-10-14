//! Core of EverythingOS: the in-memory file index, the Everything-style query
//! language and the parallel search engine. This crate does no I/O besides
//! reading and writing index snapshots.

pub mod fold;
pub mod perm;
pub mod query;
pub mod search;
pub mod sort;
pub mod store;
pub mod wildcard;

pub use store::{EntryId, Index, ListedEntry, ListingOutcome, Meta, NO_ENTRY, ROOT};
