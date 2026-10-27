use hidshim::{Buttons, Gamepad, State, Status};
use std::time::{Duration, Instant};
use std::{env, io, thread};
use windows::Gaming::Input::Gamepad as WgiGamepad;
use windows_sys::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

mod autostart;
mod setup;
mod window;

const USAGE: &str = "usage: hidshim <command>

  install [--no-autostart]
                   install or update hidshim and its driver, or bring the
                   pads back after a reboot; unless told otherwise, also
                   bring them back on every boot (asks for administrator
                   rights)
  uninstall        remove hidshim, its driver, certificate and boot task
                   (asks for administrator rights)
  autostart [on|off]
                   whether the pads come back on boot, or turn that on or
                   off (asks for administrator rights)
  status           whether the driver is installed and the pads are running
  list             the pads and the processes holding them
  pad [seconds]    hold a pad and play a test pattern (default 30)
  watch [seconds]  print what XInput and Windows.Gaming.Input see (default 30)
  bench [count]    latency from submit to XInputGetState (default 1000)";

#[derive(Debug, PartialEq)]
enum Command {
    Install { autostart: bool },
    Uninstall,
    Autostart(Option<bool>),
    Restore,
    Status,
    List,
    Pad(Duration),
    Watch(Duration),
    Bench(usize),
}

fn parse(args: &[String]) -> Option<Command> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let number = |default: u64| match args.get(1) {
        None => Some(default),
        Some(text) => text.parse().ok(),
    };
    if args.len() > 2 {
        return None;
    }
    Some(match *args.first()? {
        "install" => match args.get(1) {
            None => Command::Install { autostart: true },
            Some(&"--no-autostart") => Command::Install { autostart: false },
            Some(_) => return None,
        },
        "uninstall" if args.len() == 1 => Command::Uninstall,
        "autostart" => Command::Autostart(match args.get(1) {
            None => None,
            Some(&"on") => Some(true),
            Some(&"off") => Some(false),
            Some(_) => return None,
        }),
        // Run by the boot task, as SYSTEM.
        "restore" if args.len() == 1 => Command::Restore,
        "status" if args.len() == 1 => Command::Status,
        "list" if args.len() == 1 => Command::List,
        "pad" => Command::Pad(Duration::from_secs(number(30)?)),
        "watch" => Command::Watch(Duration::from_secs(number(30)?)),
        "bench" => Command::Bench(usize::try_from(number(1000)?).ok()?),
        _ => return None,
    })
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(command) = parse(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let result = match command {
        Command::Install { autostart } => elevated(|| setup::install(autostart)),
        Command::Uninstall => elevated(setup::uninstall),
        Command::Autostart(None) => autostart::enabled().map(|on| {
            println!("{}", if on { "on" } else { "off" });
        }),
        Command::Autostart(Some(on)) => elevated(|| setup::set_autostart(on)),
        Command::Restore => hidshim::restore(),
        Command::Status => status(),
        Command::List => list(),
        Command::Pad(duration) => pad(duration),
        Command::Watch(duration) => watch(duration),
        Command::Bench(count) => bench(count),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn elevated(run: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
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
            Status::NotPresent => "installed, pads not running (run install to bring them back)",
            Status::Ready => "ready",
        }
    );
    Ok(())
}

fn list() -> io::Result<()> {
    let pads = hidshim::pads()?;
    if pads.is_empty() {
        println!("no pads running");
    }
    for pad in pads {
        match pad.owner {
            None => println!("pad {}: free", pad.index),
            Some(pid) => match setup::process_name(pid) {
                Some(name) => println!("pad {}: held by {name} ({pid})", pad.index),
                None => println!("pad {}: held by process {pid}", pad.index),
            },
        }
    }
    Ok(())
}

fn pad(duration: Duration) -> io::Result<()> {
    let mut pad = Gamepad::open()?;
    println!("pad {} open for {}s", pad.index(), duration.as_secs());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Option<Command> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse(&args)
    }

    #[test]
    fn install_sets_up_autostart_unless_told_not_to() {
        assert_eq!(
            parsed("install"),
            Some(Command::Install { autostart: true })
        );
        assert_eq!(
            parsed("install --no-autostart"),
            Some(Command::Install { autostart: false })
        );
        assert_eq!(parsed("install --autostart"), None);
    }

    #[test]
    fn autostart_reports_or_switches() {
        assert_eq!(parsed("autostart"), Some(Command::Autostart(None)));
        assert_eq!(parsed("autostart on"), Some(Command::Autostart(Some(true))));
        assert_eq!(
            parsed("autostart off"),
            Some(Command::Autostart(Some(false)))
        );
        assert_eq!(parsed("autostart yes"), None);
    }

    #[test]
    fn numbers_default_and_must_parse() {
        assert_eq!(parsed("pad"), Some(Command::Pad(Duration::from_secs(30))));
        assert_eq!(
            parsed("watch 5"),
            Some(Command::Watch(Duration::from_secs(5)))
        );
        assert_eq!(parsed("bench"), Some(Command::Bench(1000)));
        assert_eq!(parsed("bench many"), None);
    }

    #[test]
    fn unknown_or_extra_arguments_show_usage() {
        assert_eq!(parsed(""), None);
        assert_eq!(parsed("frobnicate"), None);
        assert_eq!(parsed("status now"), None);
        assert_eq!(parsed("pad 1 2"), None);
        assert_eq!(parsed("restore"), Some(Command::Restore));
        assert_eq!(parsed("list"), Some(Command::List));
    }
}
