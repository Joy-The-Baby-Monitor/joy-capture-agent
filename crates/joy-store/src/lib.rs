//! Persistence: configuration and secrets.
//!
//! `joy-store` keeps layered settings (built-in defaults < on-disk TOML <
//! runtime overrides) separate from secrets (WiFi password, pairing keys,
//! paired-client tokens), which are stored apart and encrypted at rest using
//! device-unique key material. Structured needs such as event history and the
//! access list graduate to SQLite when a flat file is no longer enough.
//!
//! This crate is currently a scaffold; the config merge and secret store land as
//! settings and the trust model are built out.
