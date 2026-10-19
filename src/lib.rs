//! queuewatch: a collector that holds the *arr API keys and turns what the
//! services report into a key-free [`snapshot::Snapshot`], and a page that
//! shows that snapshot to people signed in through OIDC.

pub mod collect;
pub mod config;
pub mod serve;
pub mod snapshot;
