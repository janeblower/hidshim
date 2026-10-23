//! Virtual Xbox 360 pad for XInput and Windows.Gaming.Input, backed by a
//! user-mode driver.
//!
//! [`install`] needs administrator rights and is run once. [`Gamepad::open`]
//! does not: the pad appears when it is opened and disappears when it is
//! dropped.
#![cfg(windows)]

mod device;
mod gamepad;
mod install;
mod pnp;
mod proto;
mod sign;
mod win;

pub use gamepad::{Buttons, Gamepad, State};
pub use install::{install, uninstall};
