//! The virtual HID keyboard: keys, consumer keys and a mouse, delivered as
//! HID reports through a ring the driver drains.

use crate::device::{self, Device};
use crate::kbdproto::{
    self, KEYBOARD_LEN, PROTOCOL_VERSION, REPORT_CONSUMER, REPORT_KEYBOARD, REPORT_MOUSE, Shared,
    Slot,
};
use crate::pnp::{Status, our_devices};
use crate::proto::{IOCTL_GET_VERSION, IOCTL_MAP_REPORT, Mapping, Version};
use crate::ring;
use crate::shared::Section;
use std::io;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::GUID_DEVCLASS_SYSTEM;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_DEVICE_NOT_CONNECTED};

/// HID usages of the keyboard page, for [`Keys::set`].
pub mod usage {
    pub const A: u8 = 0x04;
    pub const B: u8 = 0x05;
    pub const C: u8 = 0x06;
    pub const D: u8 = 0x07;
    pub const E: u8 = 0x08;
    pub const F: u8 = 0x09;
    pub const G: u8 = 0x0A;
    pub const H: u8 = 0x0B;
    pub const I: u8 = 0x0C;
    pub const J: u8 = 0x0D;
    pub const K: u8 = 0x0E;
    pub const L: u8 = 0x0F;
    pub const M: u8 = 0x10;
    pub const N: u8 = 0x11;
    pub const O: u8 = 0x12;
    pub const P: u8 = 0x13;
    pub const Q: u8 = 0x14;
    pub const R: u8 = 0x15;
    pub const S: u8 = 0x16;
    pub const T: u8 = 0x17;
    pub const U: u8 = 0x18;
    pub const V: u8 = 0x19;
    pub const W: u8 = 0x1A;
    pub const X: u8 = 0x1B;
    pub const Y: u8 = 0x1C;
    pub const Z: u8 = 0x1D;
    pub const D1: u8 = 0x1E;
    pub const D2: u8 = 0x1F;
    pub const D3: u8 = 0x20;
    pub const D4: u8 = 0x21;
    pub const D5: u8 = 0x22;
    pub const D6: u8 = 0x23;
    pub const D7: u8 = 0x24;
    pub const D8: u8 = 0x25;
    pub const D9: u8 = 0x26;
    pub const D0: u8 = 0x27;
    pub const ENTER: u8 = 0x28;
    pub const ESCAPE: u8 = 0x29;
    pub const BACKSPACE: u8 = 0x2A;
    pub const TAB: u8 = 0x2B;
    pub const SPACE: u8 = 0x2C;
    pub const MINUS: u8 = 0x2D;
    pub const EQUAL: u8 = 0x2E;
    pub const LEFT_BRACKET: u8 = 0x2F;
    pub const RIGHT_BRACKET: u8 = 0x30;
    pub const BACKSLASH: u8 = 0x31;
    pub const SEMICOLON: u8 = 0x33;
    pub const APOSTROPHE: u8 = 0x34;
    pub const GRAVE: u8 = 0x35;
    pub const COMMA: u8 = 0x36;
    pub const PERIOD: u8 = 0x37;
    pub const SLASH: u8 = 0x38;
    pub const CAPS_LOCK: u8 = 0x39;
    pub const F1: u8 = 0x3A;
    pub const F2: u8 = 0x3B;
    pub const F3: u8 = 0x3C;
    pub const F4: u8 = 0x3D;
    pub const F5: u8 = 0x3E;
    pub const F6: u8 = 0x3F;
    pub const F7: u8 = 0x40;
    pub const F8: u8 = 0x41;
    pub const F9: u8 = 0x42;
    pub const F10: u8 = 0x43;
    pub const F11: u8 = 0x44;
    pub const F12: u8 = 0x45;
    pub const F13: u8 = 0x68;
    pub const F14: u8 = 0x69;
    pub const F15: u8 = 0x6A;
    pub const F16: u8 = 0x6B;
    pub const F17: u8 = 0x6C;
    pub const F18: u8 = 0x6D;
    pub const F19: u8 = 0x6E;
    pub const F20: u8 = 0x6F;
    pub const F21: u8 = 0x70;
    pub const F22: u8 = 0x71;
    pub const F23: u8 = 0x72;
    pub const F24: u8 = 0x73;
    pub const DELETE: u8 = 0x4C;
    pub const RIGHT: u8 = 0x4F;
    pub const LEFT: u8 = 0x50;
    pub const DOWN: u8 = 0x51;
    pub const UP: u8 = 0x52;
    pub const LEFT_CTRL: u8 = 0xE0;
    pub const LEFT_SHIFT: u8 = 0xE1;
    pub const LEFT_ALT: u8 = 0xE2;
    pub const LEFT_GUI: u8 = 0xE3;
    pub const RIGHT_CTRL: u8 = 0xE4;
    pub const RIGHT_SHIFT: u8 = 0xE5;
    pub const RIGHT_ALT: u8 = 0xE6;
    pub const RIGHT_GUI: u8 = 0xE7;
}

/// HID usages of the consumer page, for [`KeyboardState::consumer`].
pub mod consumer {
    pub const NEXT: u16 = 0xB5;
    pub const PREVIOUS: u16 = 0xB6;
    pub const PLAY_PAUSE: u16 = 0xCD;
    pub const MUTE: u16 = 0xE2;
    pub const VOLUME_UP: u16 = 0xE9;
    pub const VOLUME_DOWN: u16 = 0xEA;
}

/// First and last usage of the keyboard page a report can carry.
const FIRST_KEY: u8 = 0x04;
const LAST_KEY: u8 = 0xE7;
/// Bytes of the NKRO map in a report, covering usages 0 to 0xDF.
const MAP_LEN: usize = KEYBOARD_LEN - 2;
/// The consumer collection declares usages 0 to this.
const LAST_CONSUMER: u16 = 0x3FF;
/// The mouse axes declare a logical minimum of -32767 and -127.
const AXIS_MIN: i16 = -32767;
const WHEEL_MIN: i8 = -127;
const BUTTON_MASK: u8 = 0x1F;

/// The keys held down, by usage of the keyboard page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keys([u8; 32]);

impl Keys {
    /// Presses or releases `usage`. Usages outside `0x04..=0xE7` cannot be
    /// reported and are ignored; `0xE0..=0xE7` are the modifiers.
    pub fn set(&mut self, usage: u8, down: bool) {
        if !(FIRST_KEY..=LAST_KEY).contains(&usage) {
            return;
        }
        let (byte, bit) = (usize::from(usage / 8), usage % 8);
        if down {
            self.0[byte] |= 1 << bit;
        } else {
            self.0[byte] &= !(1 << bit);
        }
    }

    pub fn contains(&self, usage: u8) -> bool {
        self.0[usize::from(usage / 8)] & (1 << (usage % 8)) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == [0; 32]
    }
}

/// Mouse buttons, bits of [`KeyboardState::buttons`].
pub mod button {
    pub const LEFT: u8 = 0x01;
    pub const RIGHT: u8 = 0x02;
    pub const MIDDLE: u8 = 0x04;
    pub const BACK: u8 = 0x08;
    pub const FORWARD: u8 = 0x10;
}

/// Everything the virtual keyboard holds down: a state, so submitting it
/// again changes nothing. Mouse movement is not part of it, see
/// [`Keyboard::motion`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardState {
    pub keys: Keys,
    /// Held usages of the consumer page, `0` for an empty place. Values
    /// above `0x3FF` are treated as `0`.
    pub consumer: [u16; 4],
    /// Held mouse buttons, see [`button`]; bits above the fifth are ignored.
    pub buttons: u8,
}

/// What the driver was last given, to send only what changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sent {
    keys: Keys,
    consumer: [u16; 4],
    buttons: u8,
}

/// Up to three reports of one `submit`.
#[derive(Debug, Default)]
struct Batch {
    slots: [Slot; 3],
    len: usize,
}

impl Batch {
    fn push(&mut self, slot: Slot) {
        self.slots[self.len] = slot;
        self.len += 1;
    }

    fn as_slice(&self) -> &[Slot] {
        &self.slots[..self.len]
    }
}

fn clean_consumer(usages: [u16; 4]) -> [u16; 4] {
    usages.map(|u| if u <= LAST_CONSUMER { u } else { 0 })
}

/// The reports that take the driver from `sent` to `state`, and the new
/// `sent`: keyboard, consumer keys and mouse buttons, each when it changed.
fn diff(sent: &Sent, state: &KeyboardState) -> (Batch, Sent) {
    let mut batch = Batch::default();
    let next = Sent {
        keys: state.keys,
        consumer: clean_consumer(state.consumer),
        buttons: state.buttons & BUTTON_MASK,
    };
    if next.keys != sent.keys {
        batch.push(keyboard_report(&next.keys));
    }
    if next.consumer != sent.consumer {
        batch.push(consumer_report(&next.consumer));
    }
    if next.buttons != sent.buttons {
        batch.push(mouse_report(next.buttons, Motion::default()));
    }
    (batch, next)
}

/// `[01] [modifiers] [28-byte map]`; bit `u` of the map is usage `u`.
fn keyboard_report(keys: &Keys) -> Slot {
    let mut slot = [0; 32];
    slot[0] = REPORT_KEYBOARD;
    slot[1] = keys.0[usize::from(0xE0u8 / 8)];
    slot[2..2 + MAP_LEN].copy_from_slice(&keys.0[..MAP_LEN]);
    slot
}

fn consumer_report(usages: &[u16; 4]) -> Slot {
    let mut slot = [0; 32];
    slot[0] = REPORT_CONSUMER;
    for (i, usage) in usages.iter().enumerate() {
        slot[1 + 2 * i..3 + 2 * i].copy_from_slice(&usage.to_le_bytes());
    }
    slot
}

/// A relative movement, as [`Keyboard::motion`] takes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Motion {
    dx: i16,
    dy: i16,
    wheel: i8,
    hwheel: i8,
}

fn mouse_report(buttons: u8, mouse: Motion) -> Slot {
    let mut slot = [0; 32];
    slot[0] = REPORT_MOUSE;
    slot[1] = buttons;
    slot[2..4].copy_from_slice(&mouse.dx.max(AXIS_MIN).to_le_bytes());
    slot[4..6].copy_from_slice(&mouse.dy.max(AXIS_MIN).to_le_bytes());
    slot[6] = mouse.wheel.max(WHEEL_MIN) as u8;
    slot[7] = mouse.hwheel.max(WHEEL_MIN) as u8;
    slot
}

/// How long `open` waits for the HID side to attach, as right after a boot
/// or an install.
const ATTACH_WAIT: Duration = Duration::from_secs(5);

/// The virtual keyboard, mouse and media keys. Windows sees one device while
/// this value is alive; dropping it releases everything held.
///
/// Needs SYSTEM or administrator rights, because the input is hardware-like
/// and reaches the secure desktop.
#[derive(Debug)]
pub struct Keyboard {
    /// Declared before device: it is dropped first and borrows its handle.
    section: Section<Shared>,
    device: Device,
    sent: Sent,
    head: u32,
}

impl Keyboard {
    /// Claims the keyboard. Blocks for up to 5 seconds while the HID side
    /// attaches, as right after a boot or an install, so do not call it from
    /// a UI or async thread. Fails with [`io::ErrorKind::NotFound`] when it is
    /// not running, see [`Keyboard::status`], with
    /// [`io::ErrorKind::PermissionDenied`] without SYSTEM or administrator
    /// rights, with [`io::ErrorKind::ResourceBusy`] when another process
    /// holds it, and with [`io::ErrorKind::NotConnected`] when its HID side
    /// does not attach.
    pub fn open() -> io::Result<Self> {
        let interface = device::keyboard(device::CONTROL)?.ok_or_else(not_found)?;
        let mut device = interface.open().map_err(open_error)?;
        let version: Version = device.query(IOCTL_GET_VERSION)?;
        if version.version != PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "hidshim keyboard driver speaks protocol {}, expected {PROTOCOL_VERSION}",
                version.version
            )));
        }
        let mapping = map_report(&mut device)?;
        let section = Section::<Shared>::with_mapping(&mut device, mapping)?;
        // SAFETY: the view maps a whole Shared; only this process writes it.
        let head = unsafe { (*section.view()).head.load(Ordering::Relaxed) };
        Ok(Self {
            section,
            device,
            sent: Sent::default(),
            head,
        })
    }

    /// Makes Windows see `state`: the reports of whatever changed since the
    /// last call, in the order keyboard, consumer keys, mouse buttons.
    /// Windows receives them in that order within a collection; across
    /// collections it does not promise one, so space calls apart when it
    /// matters.
    ///
    /// Fails with [`io::ErrorKind::WouldBlock`] when the driver has not taken
    /// 64 reports yet: nothing is sent and the next call compares with the
    /// same state as this one did. Fails with [`io::ErrorKind::NotConnected`]
    /// once the keyboard was removed or its HID side went away; open it
    /// again.
    pub fn submit(&mut self, state: &KeyboardState) -> io::Result<()> {
        let (batch, next) = diff(&self.sent, state);
        self.send(&batch)?;
        self.sent = next;
        Ok(())
    }

    /// Moves the pointer by `dx`, `dy` and scrolls the wheel up by `wheel`
    /// and right by `hwheel`, in one report with the buttons held now. All
    /// zero sends nothing. Errors as [`Keyboard::submit`]; the movement of a
    /// failed call is lost.
    pub fn motion(&mut self, dx: i16, dy: i16, wheel: i8, hwheel: i8) -> io::Result<()> {
        let motion = Motion {
            dx,
            dy,
            wheel,
            hwheel,
        };
        if motion == Motion::default() {
            return Ok(());
        }
        let mut batch = Batch::default();
        batch.push(mouse_report(self.sent.buttons, motion));
        self.send(&batch)
    }

    fn send(&mut self, batch: &Batch) -> io::Result<()> {
        if !self.section.alive(&mut self.device) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the hidshim keyboard was removed",
            ));
        }
        if batch.len == 0 {
            return Ok(());
        }
        // SAFETY: the view maps a whole Shared, and this value is its only
        // writer.
        let wake = unsafe { ring::publish(self.section.view(), &mut self.head, batch.as_slice()) }
            .map_err(|ring::Full| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the hidshim keyboard has 64 reports Windows has not read",
                )
            })?;
        if wake {
            self.section.wake();
        }
        Ok(())
    }

    /// Whether the keyboard is installed and running. Needs no elevation.
    pub fn status() -> io::Result<Status> {
        if device::keyboard(device::CONTROL)?.is_some() {
            return Ok(Status::Ready);
        }
        let (_set, devices) =
            our_devices(&GUID_DEVCLASS_SYSTEM, &[kbdproto::CONTROL_HARDWARE_ID], 0)?;
        Ok(if devices.is_empty() {
            Status::NotInstalled
        } else {
            Status::NotPresent
        })
    }

    /// Who holds the keyboard and whether its HID side is attached. Needs
    /// administrator rights; claims nothing.
    pub fn info() -> io::Result<KeyboardInfo> {
        let interface = device::keyboard(device::ADMIN)?.ok_or_else(not_found)?;
        let mut device = interface.open().map_err(open_error)?;
        let info: kbdproto::Info = device.query(kbdproto::IOCTL_GET_INFO)?;
        Ok(KeyboardInfo {
            owner: (info.owned != 0).then_some(info.owner_process_id),
            hid_connected: info.hid_connected != 0,
        })
    }
}

/// The keyboard as [`Keyboard::info`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyboardInfo {
    /// The process holding the keyboard, `None` while it is free.
    pub owner: Option<u32>,
    /// Whether the HID device is up. Without it the keyboard cannot be opened.
    pub hid_connected: bool,
}

/// The HID side attaches asynchronously to the control device, so a first
/// claim right after boot or install may find it missing for a moment.
fn map_report(device: &mut Device) -> io::Result<Mapping> {
    let start = Instant::now();
    loop {
        match device.query(IOCTL_MAP_REPORT) {
            Err(e) if e.raw_os_error() == Some(ERROR_DEVICE_NOT_CONNECTED as i32) => {
                if start.elapsed() >= ATTACH_WAIT {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "the hidshim keyboard's HID device is not attached",
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

fn open_error(e: io::Error) -> io::Error {
    match e.raw_os_error().map(|code| code as u32) {
        Some(ERROR_BUSY) => io::Error::new(
            io::ErrorKind::ResourceBusy,
            "the hidshim keyboard is already in use",
        ),
        Some(ERROR_ACCESS_DENIED) => io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the hidshim keyboard needs SYSTEM or administrator rights",
        ),
        _ => e,
    }
}

fn not_found() -> io::Error {
    let message = match Keyboard::status() {
        Ok(Status::NotInstalled) => "the hidshim keyboard is not installed; install hidshim again",
        _ => "the hidshim keyboard is not running; installing hidshim again restores it",
    };
    io::Error::new(io::ErrorKind::NotFound, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kbdproto::{CONSUMER_LEN, MOUSE_LEN};

    fn state(keys: &[u8]) -> KeyboardState {
        let mut state = KeyboardState::default();
        for &key in keys {
            state.keys.set(key, true);
        }
        state
    }

    /// The reports of a `diff`, each cut to the length its ID implies.
    fn reports(sent: &Sent, state: &KeyboardState) -> (Vec<Vec<u8>>, Sent) {
        let (batch, next) = diff(sent, state);
        let length = |slot: &Slot| match slot[0] {
            REPORT_KEYBOARD => KEYBOARD_LEN,
            REPORT_CONSUMER => CONSUMER_LEN,
            _ => MOUSE_LEN,
        };
        let out = batch
            .as_slice()
            .iter()
            .map(|slot| slot[..length(slot)].to_vec())
            .collect();
        (out, next)
    }

    fn ids(reports: &[Vec<u8>]) -> Vec<u8> {
        reports.iter().map(|r| r[0]).collect()
    }

    #[test]
    fn a_key_sets_its_bit_of_the_map() {
        let (out, _) = reports(&Sent::default(), &state(&[usage::A, usage::D, usage::F12]));
        let mut expected = vec![0u8; KEYBOARD_LEN];
        expected[0] = 1;
        // Usage 4 is bit 4 of map byte 0, usage 7 bit 7, 0x45 bit 5 of byte 8.
        expected[2] = 0x10 | 0x80;
        expected[2 + 8] = 0x20;
        assert_eq!(out, vec![expected]);
    }

    #[test]
    fn modifiers_go_to_their_own_byte_and_not_into_the_map() {
        let (out, _) = reports(
            &Sent::default(),
            &state(&[usage::LEFT_SHIFT, usage::RIGHT_GUI, usage::A]),
        );
        let report = &out[0];
        assert_eq!(report[1], 0x02 | 0x80);
        assert_eq!(report[2], 0x10);
        assert!(report[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn usages_that_cannot_be_reported_are_ignored() {
        let mut keys = Keys::default();
        for usage in [0, 1, 2, 3, 0xE8, 0xFF] {
            keys.set(usage, true);
        }
        assert!(keys.is_empty());
        keys.set(0x04, true);
        keys.set(0xE7, true);
        assert!(keys.contains(0x04) && keys.contains(0xE7));
        keys.set(0x04, false);
        assert!(!keys.contains(0x04));
    }

    #[test]
    fn consumer_usages_are_little_endian_places() {
        let state = KeyboardState {
            consumer: [consumer::VOLUME_UP, 0x0203, 0, 0xFFFF],
            ..KeyboardState::default()
        };
        let (out, _) = reports(&Sent::default(), &state);
        assert_eq!(out, vec![vec![2, 0xE9, 0, 0x03, 0x02, 0, 0, 0, 0]]);
    }

    #[test]
    fn button_report_follows_the_descriptor() {
        let state = KeyboardState {
            buttons: button::LEFT | button::FORWARD | 0xE0,
            ..KeyboardState::default()
        };
        let (out, _) = reports(&Sent::default(), &state);
        assert_eq!(out, vec![vec![3, 0x11, 0, 0, 0, 0, 0, 0]]);
    }

    #[test]
    fn motion_report_follows_the_descriptor() {
        let slot = mouse_report(
            button::RIGHT,
            Motion {
                dx: -2,
                dy: 0x0102,
                wheel: -1,
                hwheel: 5,
            },
        );
        assert_eq!(
            slot[..MOUSE_LEN],
            [3, 0x02, 0xFE, 0xFF, 0x02, 0x01, 0xFF, 5]
        );
    }

    #[test]
    fn axes_stay_inside_the_declared_range() {
        let slot = mouse_report(
            0,
            Motion {
                dx: i16::MIN,
                dy: i16::MIN,
                wheel: i8::MIN,
                hwheel: i8::MIN,
            },
        );
        assert_eq!(
            slot[..MOUSE_LEN],
            [3, 0, 0x01, 0x80, 0x01, 0x80, 0x81, 0x81]
        );
    }

    #[test]
    fn an_unchanged_state_sends_nothing() {
        let held = state(&[usage::A]);
        let (_, sent) = reports(&Sent::default(), &held);
        assert!(reports(&sent, &held).0.is_empty());
        assert!(
            reports(&Sent::default(), &KeyboardState::default())
                .0
                .is_empty()
        );
    }

    #[test]
    fn only_the_changed_parts_are_sent() {
        let mut held = state(&[usage::A]);
        held.consumer[0] = consumer::MUTE;
        held.buttons = button::RIGHT;
        let (first, sent) = reports(&Sent::default(), &held);
        assert_eq!(ids(&first), [1, 2, 3]);

        let mut next = held;
        next.keys.set(usage::B, true);
        let (out, sent) = reports(&sent, &next);
        assert_eq!(ids(&out), [1]);

        next.consumer[0] = 0;
        let (out, sent) = reports(&sent, &next);
        assert_eq!(ids(&out), [2]);

        next.buttons = 0;
        let (out, _) = reports(&sent, &next);
        assert_eq!(ids(&out), [3]);
    }

    #[test]
    fn resubmitting_a_state_never_moves_the_pointer() {
        let held = KeyboardState {
            buttons: button::LEFT,
            ..KeyboardState::default()
        };
        let (_, sent) = reports(&Sent::default(), &held);
        for _ in 0..3 {
            assert!(reports(&sent, &held).0.is_empty());
        }
    }

    #[test]
    fn snap_tap_is_a_release_then_a_press_in_two_submits() {
        let (_, sent) = reports(&Sent::default(), &state(&[usage::D]));
        let (release, sent) = reports(&sent, &state(&[]));
        assert_eq!(release.len(), 1);
        assert_eq!(release[0][2], 0);
        let (press, _) = reports(&sent, &state(&[usage::A]));
        assert_eq!(press[0][2], 0x10);
    }

    #[test]
    fn the_layout_constants_agree_with_the_driver_header() {
        assert_eq!(MAP_LEN, 28);
        assert_eq!(CONSUMER_LEN, 1 + 4 * 2);
        assert_eq!(MOUSE_LEN, 8);
    }
}
