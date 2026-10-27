//! Virtual Xbox 360 pad for XInput and Windows.Gaming.Input, backed by a
//! user-mode driver.
//!
//! [`Gamepad::open`] needs no elevation: it claims one of the installed pads,
//! which appears when it is opened and disappears when it is dropped.
//! [`status`] tells whether the driver is installed and running, and [`pads`]
//! which pads are held and by whom. Installing takes the `install` feature, which builds
//! the driver into the crate, and administrator rights.
#![cfg(windows)]

mod device;
mod gamepad;
#[cfg(feature = "install")]
mod install;
mod pnp;
mod proto;
mod shared;
#[cfg(feature = "install")]
mod sign;
#[cfg(feature = "install")]
mod store;
mod win;

pub use gamepad::{Buttons, Gamepad, Pad, State, pads};
#[cfg(feature = "install")]
pub use install::{install, restore, uninstall};
pub use pnp::{Status, status};
