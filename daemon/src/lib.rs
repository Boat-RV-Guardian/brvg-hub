// The hub, as a library — the binary in main.rs is one caller; the desktop app is (transitionally)
// the other, for its `--hub` compatibility path. See Cargo.toml for why this is its own crate.

pub mod hub_config;
pub mod cycle;
pub mod linktap_runtime;
pub mod linktap;
pub mod hub_relay;
pub mod hub_server;
pub mod tray_state;
