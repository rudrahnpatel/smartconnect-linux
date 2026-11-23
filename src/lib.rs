//! A Linux peer for Motorola Smart Connect that the unmodified phone app treats as a
//! Windows PC. Protocol notes: `evidence-2026-10-20/protocol-findings.md`.

pub mod app_icons;
pub mod ble;
pub mod ble_connect;
pub mod cable_link;
pub mod clipboard;
pub mod control;
pub mod discovery;
pub mod fex_host;
pub mod helpers;
pub mod hotspot;
pub mod identity;
pub mod invite;
pub mod notifications;
pub mod pairing;
pub mod pc_lock;
pub mod proto;
pub mod reconnect;
pub mod server;
pub mod sms;
pub mod status;
pub mod webcam;
