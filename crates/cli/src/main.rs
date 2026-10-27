use hidshim::{Buttons, Gamepad, State, Status};
use std::time::{Duration, Instant};
use std::{env, io, thread};
use windows::Gaming::Input::Gamepad as WgiGamepad;
use windows_sys::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

mod setup;
mod window;

const USAGE: &str = "usage: hidshim <command>

  install          install or update hidshim and its driver, or bring the
                   pad back after a reboot (asks for administrator rights)
  uninstall        remove hidshim, its driver and certificate (asks for
                   administrator rights)
  status           whether the driver is installed and the pad is running
  pad [seconds]    hold the pad and play a test pattern (default 30)
  watch [seconds]  print what XInput and Windows.Gaming.Input see (default 30)
  bench [count]    latency from submit to XInputGetState (default 1000)";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let number = |default: u64| args.get(1).and_then(|s| s.parse().ok()).unwrap_or(default);
    let result = match args.first().map(String::as_str) {
        Some("install") => elevated(setup::install),
        Some("uninstall") => elevated(setup::uninstall),
        Some("status") => status(),
        Some("pad") => pad(Duration::from_secs(number(30))),
        Some("watch") => watch(Duration::from_secs(number(30))),
        Some("bench") => bench(number(1000) as usize),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn elevated(run: fn() -> io::Result<()>) -> io::Result<()> {
    match setup::relaunch_elevated()? {
        None => run(),
        Some(0) => Ok(()),
        Some(code) => Err(io::Error::other(format!(
            "the elevated hidshim failed with exit code {code}"
        ))),
    }
}

fn status() -> io::Result<()> {
    println!(
        "{}",
        match hidshim::status()? {
            Status::NotInstalled => "not installed",
            Status::NotPresent => "installed, pad not running (run install to bring it back)",
            Status::Ready => "ready",
        }
    );
    Ok(())
}

fn pad(duration: Duration) -> io::Result<()> {
    let mut pad = Gamepad::open()?;
    println!("pad open for {}s", duration.as_secs());
    let start = Instant::now();
    while start.elapsed() < duration {
        let t = start.elapsed().as_secs_f32();
        let (sin, cos) = (t * std::f32::consts::TAU / 2.0).sin_cos();
        let buttons = if (t as u32).is_multiple_of(2) {
            Buttons::A
        } else {
            Buttons::default()
        };
        pad.submit(&State {
            buttons,
            left_trigger: ((t * 128.0) as u32 % 256) as u8,
            left_x: (cos * 32767.0) as i16,
            left_y: (sin * 32767.0) as i16,
            ..State::default()
        })?;
        thread::sleep(Duration::from_millis(4));
    }
    Ok(())
}

fn xinput(slot: u32) -> Option<XINPUT_STATE> {
    let mut state = XINPUT_STATE::default();
    // SAFETY: out-pointer to a local.
    (unsafe { XInputGetState(slot, &mut state) } == 0).then_some(state)
}

fn connected_slots() -> Vec<u32> {
    (0..4).filter(|&slot| xinput(slot).is_some()).collect()
}

fn watch(duration: Duration) -> io::Result<()> {
    let window = window::Window::open();
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < duration {
        let mut line = String::new();
        if let Some(window) = &window {
            window.pump();
            line += if window.focused() {
                "focused "
            } else {
                "unfocused "
            };
        }
        for slot in connected_slots() {
            let pad = xinput(slot).unwrap().Gamepad;
            line += &format!(
                "xinput[{slot}] buttons={:04x} lt={} lx={} ly={}  ",
                pad.wButtons, pad.bLeftTrigger, pad.sThumbLX, pad.sThumbLY
            );
        }
        match WgiGamepad::Gamepads().and_then(|list| list.Size()) {
            Ok(count) => {
                line += &format!("wgi={count}");
                let reading = WgiGamepad::Gamepads()
                    .and_then(|l| l.GetAt(0))
                    .and_then(|g| g.GetCurrentReading());
                if let Ok(r) = reading {
                    line += &format!(
                        " buttons={:x} lt={:.2} lx={:.2}",
                        r.Buttons.0, r.LeftTrigger, r.LeftThumbstickX
                    );
                }
            }
            Err(e) => line += &format!("wgi error {e}"),
        }
        if line != last {
            println!("{:6.2}s {line}", start.elapsed().as_secs_f32());
            last = line;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> io::Result<Duration> {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > timeout {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        thread::yield_now();
    }
    Ok(start.elapsed())
}

fn bench(count: usize) -> io::Result<()> {
    let before = connected_slots();
    let opened = Instant::now();
    let mut pad = Gamepad::open()?;
    let open = opened.elapsed();
    let mut slot = None;
    let appear = wait_for(Duration::from_secs(10), || {
        slot = connected_slots().into_iter().find(|s| !before.contains(s));
        slot.is_some()
    })?;
    let slot = slot.unwrap();
    println!("open {open:?}, then xinput slot {slot} after {appear:?}");

    let (mut submit, mut visible) = (Vec::with_capacity(count), Vec::with_capacity(count));
    for i in 0..count {
        let buttons = if i % 2 == 0 { Buttons::A } else { Buttons::B };
        let start = Instant::now();
        pad.submit(&State {
            buttons,
            ..State::default()
        })?;
        submit.push(start.elapsed());
        wait_for(Duration::from_secs(1), || {
            xinput(slot).is_some_and(|s| s.Gamepad.wButtons == buttons.0)
        })?;
        visible.push(start.elapsed());
    }

    drop(pad);
    let gone = wait_for(Duration::from_secs(10), || xinput(slot).is_none())?;
    println!("close -> slot gone after {gone:?}");
    report("submit", &mut submit);
    report("submit -> XInputGetState", &mut visible);
    Ok(())
}

fn report(name: &str, samples: &mut [Duration]) {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "{name}: p50 {:?}  p99 {:?}  max {:?}",
        at(0.5),
        at(0.99),
        at(1.0)
    );
}
