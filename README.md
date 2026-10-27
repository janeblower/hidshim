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

There are four pads, as many as XInput has slots. `Gamepad::open` claims the
first free one; a pad has one owner at a time, and it is free again once its
`Gamepad` is dropped or its process exits. `pads` lists them with the process
holding each, and claims nothing.

`status` tells whether the driver is installed and the pads are running.
`install`, `restore` and `uninstall` need administrator rights and the
`install` feature, which builds the driver into the crate; without it the SDK
is only the client above and builds without the WDK. Outside this repository,
as in a git dependency, set `HIDSHIM_DRIVER_DIR` to an unpacked
`hidshim-drivers.zip` of the matching release.

`install` signs the driver package on the machine with a fresh self-signed
certificate whose key exists only in memory during signing, trusts that
certificate in `LocalMachine\Root` and `TrustedPublisher`, and creates the
`SWD\HIDSHIM\XUSB0` to `XUSB3` software devices. `uninstall` removes all of
them.

The software devices do not survive a reboot, after which `status` reports
`NotPresent`. `restore`, or `install` again, then only recreates them, which
takes a fraction of a second.

## Build

Requires the MSVC x64 build tools. The WDK and Windows SDK come as NuGet
packages pinned by `drivers/xusb/packages.lock.json`. From a Developer
PowerShell:

```
msbuild drivers/xusb/hidshimxusb.vcxproj -restore -p:Configuration=Release -p:Platform=x64
cargo build --release
```

The driver build also stamps the INF with the workspace version and checks it
with InfVerif and the driver with ApiValidator.

## Try it

```
hidshim install            # asks for administrator rights
hidshim status
hidshim list               # the pads and who holds them
hidshim watch              # what XInput and Windows.Gaming.Input see
hidshim pad                # test pattern, in another terminal
hidshim bench              # submit -> XInputGetState latency
hidshim uninstall          # asks for administrator rights
```

`hidshim install` also copies itself to `Program Files\hidshim` and lists
hidshim among installed apps, from where it can be uninstalled. It registers a
`hidshim` task that runs `hidshim restore` as SYSTEM at boot, so the pads come
back after a reboot; `install --no-autostart` leaves it out, and
`hidshim autostart on|off` changes it later. `hidshim autostart` alone tells
whether it is on and needs no elevation.

## Test

`cargo test` needs nothing installed. The tests in `crates/sdk/tests` install,
drive and remove the real driver, so they run from an elevated prompt after
building the driver:

```
cargo test -p hidshim --features install --test driver -- --ignored --test-threads=1
```

Releases carry `hidshim.exe` with the driver inside and `hidshim-drivers.zip`
for [installing the driver by hand](docs/manual-install.md).

## License

[GPL-3.0-or-later](LICENSE).
