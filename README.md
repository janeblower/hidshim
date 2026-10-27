# hidshim

Virtual Xbox 360 pad for Windows: a user-mode (UMDF2) driver that XInput and
Windows.Gaming.Input see as a wired pad, and a Rust SDK that drives it. No
.NET, no test signing, no kernel driver.

```rust
hidshim::install()?;                      // once, as administrator
let mut pad = hidshim::Gamepad::open()?;  // no elevation
pad.submit(&hidshim::State { buttons: hidshim::Buttons::A, ..Default::default() })?;
drop(pad);                                // neutral, then gone from XInput
```

`install` signs the driver package on the machine with a fresh self-signed
certificate whose key exists only in memory during signing, trusts that
certificate in `LocalMachine\Root` and `TrustedPublisher`, and creates the
`SWD\HIDSHIM\XUSB` software device. `uninstall` removes all three.

The software device does not survive a reboot. Running `install` again
afterwards only recreates it, which takes a fraction of a second.

## Build

Requires the MSVC x64 build tools. The WDK and Windows SDK are fetched as
pinned NuGet packages into `.wdk`.

```
powershell -File drivers/build.ps1
cargo build --release
```

## Try it

```
hidshim install            # elevated
hidshim watch              # what XInput and Windows.Gaming.Input see
hidshim pad                # test pattern, in another terminal
hidshim bench              # submit -> XInputGetState latency
hidshim uninstall          # elevated
```

## License

[GPL-3.0-or-later](LICENSE).
