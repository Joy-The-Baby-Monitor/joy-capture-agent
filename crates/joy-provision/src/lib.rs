//! Provisioning and onboarding.
//!
//! `joy-provision` implements the headless-IoT onboarding flow: a BLE GATT
//! peripheral (via BlueZ) that receives WiFi credentials from the client over an
//! authenticated, app-layer-encrypted link, then applies them through
//! NetworkManager. The credential-exchange state machine sits behind a thin
//! `ProvisioningTransport` trait so a later QR option can slot in; BLE is the
//! only implementation for v1.
//!
//! This is the one Linux-only subsystem (BlueZ), tested on-device rather than on
//! macOS. This crate is currently a scaffold; the state machine lands during the
//! provisioning milestone.
