//! Control plane.
//!
//! `joy-control` handles everything that isn't media: capability negotiation,
//! get/set settings, event-kind subscription, provisioning commands, and
//! health/status queries. It is a request/response RPC surface over the control
//! channel, plus server-pushed events sourced from the event bus.
//!
//! This crate is currently a scaffold; the RPC surface lands during the
//! control-plane milestone.
