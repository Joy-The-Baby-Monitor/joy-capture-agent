//! Extension host.
//!
//! `joy-ext` embeds CPython via PyO3 and defines the extension contract:
//! extensions declare what they consume (frames, audio, event kinds) and what
//! they produce (`ext.<vendor>.*` event kinds) in a manifest, and interact only
//! through serializable message passing — never core internals. That contract
//! keeps the hosting mechanism swappable between in-process (trusted, with call
//! timeouts) and out-of-process (untrusted, isolated) without changing extension
//! or core code.
//!
//! This crate is currently a scaffold; the host and ABI land during the
//! extension milestone.
