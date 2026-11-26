//! # yanarkaze-core
//!
//! *yanar* (Azerbaijani: "burning", as in Yanardag, the mountain that never stops burning)
//! + *kaze* (Japanese 風: "wind"). A tiny async operating system that keeps burning on
//! whatever board the wind carries it to.
//!
//! This crate contains everything that does not depend on hardware: the shell, the
//! commands, the LED patterns, the button / sensor / watchdog task logic and the shared
//! state between tasks. A board crate implements the small traits in [`platform`] and
//! runs the task functions ([`shell::run`], [`led::run`], ...) on its executor.

#![no_std]
#![allow(async_fn_in_trait)]

#[macro_use]
mod term;
mod editor;
mod fun;
mod state;

pub mod button;
pub mod led;
pub mod platform;
pub mod sensor;
pub mod shell;
pub mod watchdog;

pub use platform::{BoardInfo, Button, Led, MemRegion, Reading, Sensor, System, Watchdog};

/// Version of the core, shown by `info`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
