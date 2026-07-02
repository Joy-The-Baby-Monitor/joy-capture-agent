//! Signal-processing analysis.
//!
//! `joy-analysis` hosts the always-on, model-free analyzers: frame differencing
//! plus connected-component analysis for motion (`core.motion`), and RMS energy
//! with simple voice-activity detection for sound (`core.sound`). Both emit onto
//! the event bus and are cheap enough to leave the CPU free for software encode.
//!
//! The event model is classification-agnostic, so model-based detectors can slot
//! in later as additional producers without touching this crate's contract.
//!
//! This crate is currently a scaffold; the analyzers land as the event bus is
//! built out.
