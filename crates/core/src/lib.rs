//! UI-agnostic core of the viewer: Google auth (via the gcloud CLI), the Cloud SQL
//! Admin API, a native Rust Cloud SQL connector, and read-only PostgreSQL access.

pub mod admin;
pub mod connector;
pub mod export;
pub mod gcloud;
pub mod model;
pub mod pg;
pub mod runtime;
pub mod settings;

pub use model::*;
