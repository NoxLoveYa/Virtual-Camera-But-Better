# Packs the portable build of Better Cam from a release build:
#   target\release\bundle\portable\Better-Cam-<version>-portable.zip
# holding better-cam.exe, vcam\better_cam_vcam.dll (the layout the app looks for), LICENSE and README.txt.
# It packs; it installs nothing. Build first:  npm run tauri build   (or: npm run tauri build -- --no-bundle)
# Run it with:  npm run portable
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$root = Split-Path -Parent $PSScriptRoot
$release = Join-Path $root 'target\release'
$exe = Join-Path $release 'better-cam.exe'
$dll = Join-Path $release 'better_cam_vcam.dll'
$license = Join-Path $root 'LICENSE'
$version = (Get-Content -Raw (Join-Path $root 'src-tauri\tauri.conf.json') | ConvertFrom-Json).version
if (-not $version) { throw 'No version in src-tauri\tauri.conf.json.' }

foreach ($f in $exe, $dll, $license) {
    if (-not (Test-Path -LiteralPath $f -PathType Leaf)) {
        throw "Missing $f. Run 'npm run tauri build' first."
    }
}
# A release build made before the DLL existed carries a tiny placeholder (src-tauri\build.rs); the app refuses to
# install that, so a zip with it would be useless. A real DLL starts with 'MZ' and is far larger.
$head = New-Object byte[] 2
$fs = [IO.File]::OpenRead($dll)
try { [void]$fs.Read($head, 0, 2) } finally { $fs.Dispose() }
if ((Get-Item -LiteralPath $dll).Length -lt 65536 -or $head[0] -ne 0x4D -or $head[1] -ne 0x5A) {
    throw "$dll is not a real DLL. Run 'npm run vcam:build', then 'npm run tauri build'."
}

$readme = @"
Better Cam $version (portable)
================================

Better Cam takes your webcam, applies settings and filters, and offers the result as its own virtual camera,
"Better Cam", to other apps. It needs Windows 11.

Use
---
1. Unzip this folder anywhere you can write to, and run better-cam.exe.
2. The first time, the window shows an "Install virtual camera" banner. Click it. Windows asks for administrator
   rights once: the camera is registered for the whole PC from the files in this folder
   (a copy of vcam\better_cam_vcam.dll goes to C:\Program Files\Better Cam\vcam).
   It is set up for the Windows account that clicked, and works for one account at a time.
3. Start the camera in Better Cam, then pick "Better Cam" as the camera in the other app.

Remove
------
Before you delete this folder, remove the virtual camera: Settings (top right) > Virtual camera > Remove.
That asks for administrator rights too, and deletes the camera, its registry keys, its files in
C:\Program Files\Better Cam and C:\ProgramData\BetterCam. Then close Better Cam and delete the folder.
Your saved presets are in %APPDATA%\dev.bettercam.app and the window's data in %LOCALAPPDATA%\dev.bettercam.app;
delete those by hand if you want them gone too.

NVIDIA filters
--------------
The three NVIDIA filters need a GeForce RTX card and NVIDIA's Video Effects runtime. Better Cam offers to
download the runtime from NVIDIA when you add one of them. It is never part of this zip.

Licence
-------
GNU General Public License, version 2 or later: see LICENSE. Several filters are ported from OBS Studio (GPL-2.0-or-later).
"@ -replace "`r?`n", "`r`n"

$outDir = Join-Path $release 'bundle\portable'
$zipPath = Join-Path $outDir "Better-Cam-$version-portable.zip"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }

# Entry names with forward slashes (what unzip tools expect), written by hand: Windows PowerShell's Compress-Archive
# writes backslashes.
$zip = [IO.Compression.ZipFile]::Open($zipPath, [IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($e in @(@($exe, 'better-cam.exe'), @($dll, 'vcam/better_cam_vcam.dll'), @($license, 'LICENSE'))) {
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $e[0], $e[1], [IO.Compression.CompressionLevel]::Optimal)
    }
    $entry = $zip.CreateEntry('README.txt', [IO.Compression.CompressionLevel]::Optimal)
    $w = New-Object IO.StreamWriter($entry.Open(), (New-Object Text.UTF8Encoding($false)))
    try { $w.Write($readme) } finally { $w.Dispose() }
} finally {
    $zip.Dispose()
}

Get-Item -LiteralPath $zipPath | ForEach-Object { "{0}  {1:N0} bytes" -f $_.FullName, $_.Length }
