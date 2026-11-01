//! Filesystem-native project snapshots and immutable content-addressed blobs.

mod blobs;
mod durability;
mod error;
mod format;
mod ids;
mod session;

pub use blobs::{BlobOrigin, Blobs};
pub use bytes::Bytes;
pub use error::{Error, Result};
pub use format::{StoredState, decode_state, encode_state};
pub use ids::{BlobId, DocumentId, PatchId, Revision};
pub use session::{GcReport, Session, State};

#[cfg(test)]
mod tests;
