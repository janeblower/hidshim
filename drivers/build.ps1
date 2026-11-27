# Builds the UMDF drivers into target\drivers: xusb for x64, xusb-arm64 for ARM64.
# The WDK and Windows SDK come from pinned NuGet packages, so only MSVC build
# tools need to be installed, with the ARM64 ones for -Arch arm64.

param([string[]] $Arch = @('x64'))

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$nugetVersion = '10.0.26100.6584'
$kitVersion = '10.0.26100.0'
$umdfVersion = '2.15'
$packages = [ordered]@{
    'microsoft.windows.wdk.x64'       = 'C393D03DFB640B5C92F546B32F6770EF68CD3AAF691956E7D66D8E2C28A1B55E'
    'microsoft.windows.wdk.arm64'     = 'E705B2A63EAB891DEF8F98087666F93E8F21DA8E3B5DEF81A624B83FEF5BDAE9'
    'microsoft.windows.sdk.cpp'       = '5D31B38205BDD9AC761B4CB39FBBC6B7209B01C11194324AFC674D7D119483A0'
    'microsoft.windows.sdk.cpp.x64'   = 'C29CE7A4641CB37EE32EBB8078CC65CFBABC7025076BCFBA869039204B1E960D'
    'microsoft.windows.sdk.cpp.arm64' = 'AC06922186FDD75E4FF1033A304EA629D7917BD0CA85E483762F8109584CCFC4'
}
# Per target: MSVC's name for it, the linker machine, the defines and the output folder.
$targets = @{
    'x64'   = @{ Tools = 'x64'; Machine = 'X64'; Defines = @('/D_AMD64_', '/Gz'); Out = 'xusb' }
    'arm64' = @{ Tools = 'arm64'; Machine = 'ARM64'; Defines = @('/D_ARM64_'); Out = 'xusb-arm64' }
}

function Restore-Package([string] $name) {
    $dir = Join-Path $root ".wdk\$name"
    if (Test-Path -LiteralPath $dir) { return Join-Path $dir 'c' }
    $nupkg = Join-Path $root ".wdk\$name.nupkg"
    if (-not (Test-Path -LiteralPath $nupkg)) {
        New-Item -ItemType Directory -Force (Split-Path $nupkg) | Out-Null
        Invoke-WebRequest -UseBasicParsing -OutFile $nupkg `
            -Uri "https://api.nuget.org/v3-flatcontainer/$name/$nugetVersion/$name.$nugetVersion.nupkg"
    }
    if ((Get-FileHash -LiteralPath $nupkg -Algorithm SHA256).Hash -ne $packages[$name]) {
        throw "$name checksum mismatch"
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory($nupkg, $dir)
    Join-Path $dir 'c'
}

function Invoke-Tool([string] $file, [string[]] $arguments) {
    & $file @arguments
    if ($LASTEXITCODE -ne 0) { throw "$(Split-Path -Leaf $file) failed ($LASTEXITCODE)" }
}

# Headers are the same for every target; they come with the x64 packages.
$wdkHeaders = Restore-Package 'microsoft.windows.wdk.x64'
$sdk = Restore-Package 'microsoft.windows.sdk.cpp'

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'MSVC x64 build tools not found' }
$msvcVersion = (Get-Content (Join-Path $vs 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt')).Trim()
$msvc = Join-Path $vs "VC\Tools\MSVC\$msvcVersion"
$src = Join-Path $root 'drivers\xusb'

foreach ($name in $Arch) {
    $t = $targets[$name]
    if (-not $t) { throw "unknown -Arch $name; use x64 or arm64" }
    $bin = Join-Path $msvc "bin\Hostx64\$($t.Tools)"
    if (-not (Test-Path -LiteralPath (Join-Path $bin 'cl.exe'))) { throw "MSVC $name build tools not found" }
    $wdkLib = Restore-Package "microsoft.windows.wdk.$name"
    $sdkLib = Restore-Package "microsoft.windows.sdk.cpp.$name"

    $env:INCLUDE = @(
        (Join-Path $msvc 'include'),
        (Join-Path $sdk "Include\$kitVersion\ucrt"),
        (Join-Path $sdk "Include\$kitVersion\um"),
        (Join-Path $sdk "Include\$kitVersion\shared"),
        (Join-Path $wdkHeaders "Include\wdf\umdf\$umdfVersion")
    ) -join ';'
    $env:LIB = @(
        (Join-Path $msvc "lib\$($t.Tools)"),
        (Join-Path $sdkLib "ucrt\$($t.Tools)"),
        (Join-Path $sdkLib "um\$($t.Tools)"),
        (Join-Path $wdkLib "Lib\wdf\umdf\$($t.Tools)\$umdfVersion")
    ) -join ';'

    $out = Join-Path $root "target\drivers\$($t.Out)"
    New-Item -ItemType Directory -Force $out | Out-Null
    Invoke-Tool (Join-Path $bin 'cl.exe') (@(
        # C4324 fires inside the WDK's own wdfrequest.h.
        '/nologo', '/c', '/W4', '/WX', '/wd4324', '/O2', '/GS', '/MT', '/Zi',
        '/D_WIN64', '/DUNICODE', '/D_UNICODE',
        '/DUMDF_VERSION_MAJOR=2', "/DUMDF_VERSION_MINOR=$($umdfVersion.Split('.')[1])"
    ) + $t.Defines + @("/Fo$out\", "/Fd$out\", (Join-Path $src 'hidshimxusb.c')))
    Invoke-Tool (Join-Path $bin 'link.exe') @(
        '/nologo', '/DLL', "/MACHINE:$($t.Machine)", '/DEBUG', '/OPT:REF', '/OPT:ICF',
        "/OUT:$out\hidshimxusb.dll", "/PDB:$out\hidshimxusb.pdb",
        "$out\hidshimxusb.obj", 'WdfDriverStubUm.lib', 'ntdll.lib', 'kernel32.lib', 'advapi32.lib')
    Copy-Item -Force (Join-Path $src 'hidshimxusb.inf') $out
    Write-Host "Built $out"
}
