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
mod calc;
mod editor;
mod fun;
mod game2048;
mod hw;
mod keys;
mod snake;
mod state;

pub mod button;
pub mod led;
pub mod platform;
pub mod sensor;
pub mod shell;
pub mod watchdog;

pub use platform::{BoardInfo, Button, HwError, Led, MemRegion, PinInfo, Pull, Reading, Sensor, System, Watchdog};

/// Version of the core, shown by `info`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
