//! `hidshim kbd`: checks of the virtual keyboard.

use super::{report, setup};
use hidshim::{Keyboard, KeyboardState, Mouse, Status, consumer, usage};
use std::time::{Duration, Instant};
use std::{io, thread};

/// A key press is held this long, so that slow readers see it.
const HOLD: Duration = Duration::from_millis(8);
/// How long a full ring is waited out.
const RING_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq)]
pub enum Kbd {
    Type(String),
    Media(u16),
    Wheel(i32),
    Bench(usize),
}

pub const USAGE: &str = "  kbd type <text>  type ASCII text (US layout)
  kbd media <volup|voldown|mute|play|next|prev>
                   press a media key
  kbd wheel <n>    scroll n notches, negative is down
  kbd bench [count]
                   time submit with count Snap Tap pairs (default 1000)
                   (the kbd commands need administrator rights)";

pub fn parse(args: &[&str]) -> Option<Kbd> {
    Some(match args {
        ["type", text] => Kbd::Type((*text).to_string()),
        ["media", name] => Kbd::Media(media_usage(name)?),
        ["wheel", n] => Kbd::Wheel(n.parse().ok()?),
        ["bench"] => Kbd::Bench(1000),
        ["bench", n] => Kbd::Bench(n.parse().ok()?),
        _ => return None,
    })
}

fn media_usage(name: &str) -> Option<u16> {
    Some(match name {
        "volup" => consumer::VOLUME_UP,
        "voldown" => consumer::VOLUME_DOWN,
        "mute" => consumer::MUTE,
        "play" => consumer::PLAY_PAUSE,
        "next" => consumer::NEXT,
        "prev" => consumer::PREVIOUS,
        _ => return None,
    })
}

pub fn run(command: Kbd) -> io::Result<()> {
    let mut keyboard = Keyboard::open()?;
    match command {
        Kbd::Type(text) => type_text(&mut keyboard, &text),
        Kbd::Media(code) => media(&mut keyboard, code),
        Kbd::Wheel(notches) => wheel(&mut keyboard, notches),
        Kbd::Bench(count) => bench(&mut keyboard, count),
    }
}

/// Submits, waiting while Windows has not yet read the earlier reports.
fn submit(keyboard: &mut Keyboard, state: &KeyboardState) -> io::Result<()> {
    let start = Instant::now();
    loop {
        match keyboard.submit(state) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && start.elapsed() < RING_WAIT => {
                thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

/// Shift and usage of an ASCII character on a US keyboard.
fn ascii_usage(c: char) -> Option<(bool, u8)> {
    const DIGITS: [u8; 10] = [
        usage::D0,
        usage::D1,
        usage::D2,
        usage::D3,
        usage::D4,
        usage::D5,
        usage::D6,
        usage::D7,
        usage::D8,
        usage::D9,
    ];
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    // Unshifted, shifted, usage.
    const PUNCTUATION: [(char, char, u8); 11] = [
        ('-', '_', usage::MINUS),
        ('=', '+', usage::EQUAL),
        ('[', '{', usage::LEFT_BRACKET),
        (']', '}', usage::RIGHT_BRACKET),
        ('\\', '|', usage::BACKSLASH),
        (';', ':', usage::SEMICOLON),
        ('\'', '"', usage::APOSTROPHE),
        ('`', '~', usage::GRAVE),
        (',', '<', usage::COMMA),
        ('.', '>', usage::PERIOD),
        ('/', '?', usage::SLASH),
    ];
    match c {
        'a'..='z' => Some((false, usage::A + (c as u8 - b'a'))),
        'A'..='Z' => Some((true, usage::A + (c as u8 - b'A'))),
        '0'..='9' => Some((false, DIGITS[usize::from(c as u8 - b'0')])),
        ' ' => Some((false, usage::SPACE)),
        '\n' => Some((false, usage::ENTER)),
        '\t' => Some((false, usage::TAB)),
        _ => {
            if let Some(i) = SHIFTED_DIGITS.find(c) {
                return Some((true, DIGITS[i]));
            }
            PUNCTUATION
                .iter()
                .find_map(|&(plain, shifted, key)| match c {
                    _ if c == plain => Some((false, key)),
                    _ if c == shifted => Some((true, key)),
                    _ => None,
                })
        }
    }
}

fn type_text(keyboard: &mut Keyboard, text: &str) -> io::Result<()> {
    let keys: Vec<(bool, u8)> = text
        .chars()
        .map(|c| {
            ascii_usage(c).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("cannot type {c:?}"))
            })
        })
        .collect::<io::Result<_>>()?;
    for (shift, key) in keys {
        let mut state = KeyboardState::default();
        state.keys.set(usage::LEFT_SHIFT, shift);
        state.keys.set(key, true);
        submit(keyboard, &state)?;
        thread::sleep(HOLD);
        submit(keyboard, &KeyboardState::default())?;
    }
    Ok(())
}

fn media(keyboard: &mut Keyboard, code: u16) -> io::Result<()> {
    let mut state = KeyboardState::default();
    state.consumer[0] = code;
    submit(keyboard, &state)?;
    thread::sleep(HOLD);
    submit(keyboard, &KeyboardState::default())
}

fn wheel(keyboard: &mut Keyboard, notches: i32) -> io::Result<()> {
    let mut left = notches;
    while left != 0 {
        let step = left.clamp(-127, 127);
        let state = KeyboardState {
            mouse: Mouse {
                wheel: step as i8,
                ..Mouse::default()
            },
            ..KeyboardState::default()
        };
        submit(keyboard, &state)?;
        left -= step;
        thread::sleep(HOLD);
    }
    Ok(())
}

/// Times `submit` on `count` Snap Tap pairs: each is a release of one key
/// and a press of the other, two reports in order, on F18 and F19, which do
/// nothing. Whether Windows receives every pair in order is for a reader of
/// the HID collection to say.
fn bench(keyboard: &mut Keyboard, count: usize) -> io::Result<()> {
    let with = |key: Option<u8>| {
        let mut state = KeyboardState::default();
        if let Some(key) = key {
            state.keys.set(key, true);
        }
        state
    };
    let (first, second) = (usage::F18, usage::F19);
    submit(keyboard, &with(Some(first)))?;
    let mut samples = Vec::with_capacity(count * 2);
    for i in 0..count {
        let press = if i % 2 == 0 { second } else { first };
        for state in [with(None), with(Some(press))] {
            let start = Instant::now();
            keyboard.submit(&state).or_else(|e| {
                if e.kind() == io::ErrorKind::WouldBlock {
                    submit(keyboard, &state)
                } else {
                    Err(e)
                }
            })?;
            samples.push(start.elapsed());
        }
        thread::sleep(Duration::from_millis(1));
    }
    submit(keyboard, &with(None))?;
    println!("{count} Snap Tap pairs");
    report("submit", &mut samples);
    Ok(())
}

/// The keyboard line of `status`.
pub fn status_line() -> io::Result<String> {
    let status = match Keyboard::status()? {
        Status::NotInstalled => return Ok("keyboard: not installed".into()),
        Status::NotPresent => {
            return Ok("keyboard: installed, not running (run install to bring it back)".into());
        }
        Status::Ready => "ready",
    };
    // Who holds it is for administrators to see.
    Ok(match Keyboard::info() {
        Ok(info) => {
            let hid = if info.hid_connected {
                "HID device attached"
            } else {
                "HID device not attached"
            };
            match info.owner {
                None => format!("keyboard: {status}, free, {hid}"),
                Some(pid) => match setup::process_name(pid) {
                    Some(name) => format!("keyboard: {status}, held by {name} ({pid}), {hid}"),
                    None => format!("keyboard: {status}, held by process {pid}, {hid}"),
                },
            }
        }
        Err(_) => format!("keyboard: {status}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Option<Kbd> {
        let args: Vec<&str> = line.split_whitespace().collect();
        parse(&args)
    }

    #[test]
    fn letters_and_digits_map_to_their_usages() {
        assert_eq!(ascii_usage('a'), Some((false, 0x04)));
        assert_eq!(ascii_usage('Z'), Some((true, 0x1D)));
        assert_eq!(ascii_usage('1'), Some((false, 0x1E)));
        assert_eq!(ascii_usage('0'), Some((false, 0x27)));
    }

    #[test]
    fn symbols_use_shift_on_a_us_layout() {
        assert_eq!(ascii_usage('!'), Some((true, 0x1E)));
        assert_eq!(ascii_usage(')'), Some((true, 0x27)));
        assert_eq!(ascii_usage(','), Some((false, 0x36)));
        assert_eq!(ascii_usage('?'), Some((true, 0x38)));
        assert_eq!(ascii_usage('"'), Some((true, 0x34)));
        assert_eq!(ascii_usage('_'), Some((true, 0x2D)));
        assert_eq!(ascii_usage(' '), Some((false, 0x2C)));
        assert_eq!(ascii_usage('\n'), Some((false, 0x28)));
    }

    #[test]
    fn every_printable_ascii_character_can_be_typed() {
        for c in ' '..='~' {
            assert!(ascii_usage(c).is_some(), "{c:?}");
        }
        assert_eq!(ascii_usage('é'), None);
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parsed("type hello"), Some(Kbd::Type("hello".to_string())));
        assert_eq!(parsed("media mute"), Some(Kbd::Media(consumer::MUTE)));
        assert_eq!(parsed("media loud"), None);
        assert_eq!(parsed("wheel -3"), Some(Kbd::Wheel(-3)));
        assert_eq!(parsed("wheel"), None);
        assert_eq!(parsed("bench"), Some(Kbd::Bench(1000)));
        assert_eq!(parsed("bench 5"), Some(Kbd::Bench(5)));
        assert_eq!(parsed("bench x"), None);
        assert_eq!(parsed("press"), None);
    }
}
