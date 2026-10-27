# Manual installation

`hidshim.exe install` does everything below in one step. Install by hand only
if you want to sign the driver with your own certificate or put it on a device
of your own.

`hidshim-drivers.zip` holds the driver and its INF but no catalog: Windows only
installs a driver whose catalog is signed by a certificate the machine trusts,
and hidshim does not ship one. You make and sign the catalog yourself.

All commands run in an elevated PowerShell. `Inf2Cat.exe` and `devcon.exe`
come with the [WDK](https://learn.microsoft.com/windows-hardware/drivers/download-the-wdk),
`signtool.exe` with the Windows SDK.

## 1. Catalog

Unpack the archive into a folder, then in that folder:

```powershell
Inf2Cat.exe /driver:. /os:10_X64
```

This writes `hidshimxusb.cat`.

## 2. Certificate

Skip this step if you already have a code signing certificate.

```powershell
$cert = New-SelfSignedCertificate -Type CodeSigningCert `
    -Subject 'CN=hidshim manual' -CertStoreLocation Cert:\LocalMachine\My
```

The machine has to trust it as a root and as a publisher:

```powershell
foreach ($name in 'Root', 'TrustedPublisher') {
    $store = [Security.Cryptography.X509Certificates.X509Store]::new($name, 'LocalMachine')
    $store.Open('ReadWrite'); $store.Add($cert); $store.Close()
}
```

## 3. Sign

```powershell
signtool.exe sign /sm /s My /sha1 $cert.Thumbprint /fd SHA256 hidshimxusb.cat
```

The private key is no longer needed. Deleting it means nobody can sign
anything else with a certificate your machine trusts:

```powershell
Remove-Item "Cert:\LocalMachine\My\$($cert.Thumbprint)" -DeleteKey
```

## 4. Driver and device

```powershell
pnputil /add-driver hidshimxusb.inf
```

The INF binds to the hardware ID `SWD\HIDSHIM_XUSB`. The driver does nothing
until a device with that ID exists. There are two ways to create one:

- **Software device**, as `hidshim.exe` does: call
  [`SwDeviceCreate`](https://learn.microsoft.com/windows/win32/api/swdevice/nf-swdevice-swdevicecreate)
  as an administrator, under `HTREE\ROOT\0`, with the hardware ID above and a
  container ID of your own. The device is gone after a reboot and has to be
  created again.
- **Root device**: `devcon.exe install hidshimxusb.inf SWD\HIDSHIM_XUSB`. It
  survives reboots. All root devices share a single container ID, though, so
  GameInput, and Steam through it, miss the pad appearing and disappearing.
  XInput and Windows.Gaming.Input work.

## Removal

```powershell
pnputil /remove-device /deviceid SWD\HIDSHIM_XUSB
pnputil /enum-drivers   # find the oemNN.inf whose original name is hidshimxusb.inf
pnputil /delete-driver oemNN.inf /uninstall
```

Then remove your certificate from `Root` and `TrustedPublisher`.

Programs that use Windows.Gaming.Input keep the pad open and can make Windows
ask for a reboot to remove it. `hidshim.exe uninstall` avoids that by having
the driver release the device first.
