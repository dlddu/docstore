//! docstore: a tiny document store that speaks the slice of the OpenSearch
//! REST API used by homelab-k3s-mcp, persisting each document as a JSON file.

pub mod analysis;
pub mod api;
pub mod error;
pub mod search;
pub mod store;
