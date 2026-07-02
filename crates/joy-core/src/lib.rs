//! Core vocabulary shared across the Joy Capture Agent.
//!
//! `joy-core` defines the types and traits every other crate depends on: the
//! structured event model, the capture/pipeline traits, and the configuration
//! types. It is the root of the dependency graph — every sibling crate depends
//! on it, and nothing here depends on the `joy-agentd` binary.
//!
//! Currently provides the time vocabulary ([`Timestamp`]); the event model
//! and config types land with the event-bus milestone (see the roadmap in the
//! design specification).

pub mod time;

pub use time::Timestamp;
