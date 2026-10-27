//! Virtual Xbox 360 pad for XInput and Windows.Gaming.Input, backed by a
//! user-mode driver.
//!
//! [`Gamepad::open`] needs no elevation: the pad appears when it is opened and
//! disappears when it is dropped. [`status`] tells whether the driver is
//! installed and running. Installing takes the `install` feature, which builds
//! the driver into the crate, and administrator rights.
#![cfg(windows)]

mod device;
mod gamepad;
#[cfg(feature = "install")]
mod install;
mod pnp;
mod proto;
#[cfg(feature = "install")]
mod sign;
#[cfg(feature = "install")]
mod store;
mod win;

pub use gamepad::{Buttons, Gamepad, State};
#[cfg(feature = "install")]
pub use install::{install, uninstall};
pub use pnp::{Status, status};
