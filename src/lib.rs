//! Shared core for the UTi260B+ viewer and automation CLI.
//!
//! See `UTi260B-plus-protocol-spec.md`.

pub mod app;
pub mod capture;
pub mod cli;
pub mod config;
pub mod fixtures;
pub mod frame;
pub mod net;
pub mod palette;
pub mod protocol;
pub mod replay;
pub mod usb;
pub mod usb_worker;
pub mod yuv;
