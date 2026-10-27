# hidshim

Virtual Xbox 360 pad for Windows: a user-mode (UMDF2) driver that XInput and
Windows.Gaming.Input see as a wired pad, and a Rust SDK that drives it. No
.NET, no test signing, no kernel driver.

```rust
if hidshim::status()? == hidshim::Status::Ready {
    let mut pad = hidshim::Gamepad::open()?;  // no elevation
    pad.submit(&hidshim::State { buttons: hidshim::Buttons::A, ..Default::default() })?;
    drop(pad);                                // neutral, then gone from XInput
}
```

`status` tells whether the driver is installed and the pad is running.
`install` and `uninstall` need administrator rights and the `install` feature,
which builds the driver into the crate; without it the SDK is only the client
above and builds without the WDK.

`install` signs the driver package on the machine with a fresh self-signed
certificate whose key exists only in memory during signing, trusts that
certificate in `LocalMachine\Root` and `TrustedPublisher`, and creates the
`SWD\HIDSHIM\XUSB` software device. `uninstall` removes all three.

The software device does not survive a reboot, after which `status` reports
`NotPresent`. Running `install` again then only recreates it, which takes a
fraction of a second.

## Build

Requires the MSVC x64 build tools. The WDK and Windows SDK are fetched as
pinned NuGet packages into `.wdk`.

```
powershell -File drivers/build.ps1
cargo build --release
```

## Try it

```
hidshim install            # asks for administrator rights
hidshim status
hidshim watch              # what XInput and Windows.Gaming.Input see
hidshim pad                # test pattern, in another terminal
hidshim bench              # submit -> XInputGetState latency
hidshim uninstall          # asks for administrator rights
```

`hidshim install` also copies itself to `Program Files\hidshim` and lists
hidshim among installed apps, from where it can be uninstalled. Run it again
after a reboot to bring the pad back.

Releases carry `hidshim.exe` with the driver inside and `hidshim-drivers.zip`
for [installing the driver by hand](docs/manual-install.md).

## License

[GPL-3.0-or-later](LICENSE).
