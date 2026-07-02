//! Core vocabulary shared across the Joy Capture Agent.
//!
//! `joy-core` defines the types and traits every other crate depends on: the
//! structured event model, the capture/pipeline traits, and the configuration
//! types. It is the root of the dependency graph — every sibling crate depends
//! on it, and nothing here depends on the `joy-agentd` binary.
//!
//! This crate is currently a scaffold; the concrete event, trait, and config
//! definitions land as the pipeline is built out (see the roadmap in the
//! design specification).
