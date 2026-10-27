//! Against the real driver: `cargo test -p hidshim --features install --
//! --ignored --test-threads=1` from an elevated prompt, after
//! drivers/build.ps1. Each test leaves hidshim installed.
#![cfg(all(windows, feature = "install"))]

use hidshim::{Buttons, Gamepad, State, Status};
use std::io::ErrorKind;
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

const PADS: usize = 4;

fn installed() {
    hidshim::install().expect("install");
    assert_eq!(hidshim::status().unwrap(), Status::Ready);
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn xinput_buttons() -> Vec<u16> {
    (0..4)
        .filter_map(|slot| {
            let mut state = XINPUT_STATE::default();
            // SAFETY: out-pointer to a local.
            (unsafe { XInputGetState(slot, &mut state) } == 0).then_some(state.Gamepad.wButtons)
        })
        .collect()
}

#[test]
#[ignore = "needs administrator rights and the built driver"]
fn pads_are_claimed_one_owner_each() {
    installed();
    let pid = std::process::id();
    let free = hidshim::pads().unwrap();
    assert_eq!(free.len(), PADS);
    assert!(free.iter().all(|pad| pad.owner.is_none()), "{free:?}");

    let mut held: Vec<Gamepad> = (0..PADS).map(|_| Gamepad::open().unwrap()).collect();
    let indexes: Vec<u8> = held.iter().map(Gamepad::index).collect();
    assert_eq!(indexes, [0, 1, 2, 3]);
    assert!(
        hidshim::pads()
            .unwrap()
            .iter()
            .all(|pad| pad.owner == Some(pid))
    );
    assert_eq!(Gamepad::open().unwrap_err().kind(), ErrorKind::ResourceBusy);

    drop(held.remove(2));
    let pads = hidshim::pads().unwrap();
    assert_eq!(pads[2].owner, None);
    assert_eq!(pads[1].owner, Some(pid));
    assert_eq!(Gamepad::open().unwrap().index(), 2);
}

#[test]
#[ignore = "needs administrator rights and the built driver"]
fn xinput_sees_submitted_state_until_drop() {
    installed();
    let marker = Buttons::A | Buttons::Y | Buttons::DPAD_LEFT;
    let mut pad = Gamepad::open().unwrap();
    pad.submit(&State {
        buttons: marker,
        ..State::default()
    })
    .unwrap();
    eventually("pad in XInput", || xinput_buttons().contains(&marker.0));

    pad.submit(&State {
        buttons: Buttons::B,
        ..State::default()
    })
    .unwrap();
    eventually("new state in XInput", || {
        let seen = xinput_buttons();
        seen.contains(&Buttons::B.0) && !seen.contains(&marker.0)
    });

    drop(pad);
    eventually("pad gone from XInput", || {
        !xinput_buttons().contains(&Buttons::B.0)
    });
}

#[test]
#[ignore = "needs administrator rights and the built driver"]
fn pad_outlives_the_thread_that_opened_it() {
    installed();
    let mut pad = std::thread::spawn(|| Gamepad::open().unwrap())
        .join()
        .unwrap();
    pad.submit(&State {
        buttons: Buttons::X,
        ..State::default()
    })
    .unwrap();
    eventually("state from a pad opened elsewhere", || {
        xinput_buttons().contains(&Buttons::X.0)
    });
}

#[test]
#[ignore = "needs administrator rights and the built driver"]
fn restore_and_reinstall_keep_the_pads() {
    installed();
    hidshim::restore().unwrap();
    hidshim::install().unwrap();
    assert_eq!(hidshim::pads().unwrap().len(), PADS);
}

#[test]
#[ignore = "needs administrator rights and the built driver"]
fn uninstall_removes_everything_and_frees_held_pads() {
    installed();
    let mut pad = Gamepad::open().unwrap();
    hidshim::uninstall().unwrap();
    assert_eq!(hidshim::status().unwrap(), Status::NotInstalled);
    assert!(hidshim::pads().unwrap().is_empty());
    assert_eq!(Gamepad::open().unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(
        pad.submit(&State::default()).unwrap_err().kind(),
        ErrorKind::NotConnected
    );
    // Nothing to bring back without the driver.
    hidshim::restore().unwrap();
    assert_eq!(hidshim::status().unwrap(), Status::NotInstalled);
    installed();
}
