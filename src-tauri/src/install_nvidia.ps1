$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$dir = Join-Path $env:TEMP 'better-cam-nvidia'
$failed = $false
$lock = $null
try {
    Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory $dir | Out-Null
    $exe = Join-Path $dir 'nvidia-vfx-installer.exe'

    Add-Type -AssemblyName System.Net.Http
    $client = New-Object System.Net.Http.HttpClient
    $client.Timeout = [TimeSpan]::FromMinutes(60)
    $resp = $client.GetAsync('{URL}', [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
    if (-not $resp.IsSuccessStatusCode) {
        throw ('The download server answered {0} {1}.' -f [int]$resp.StatusCode, $resp.ReasonPhrase)
    }
    $total = [long]$resp.Content.Headers.ContentLength
    $in = $resp.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
    $out = [IO.File]::Create($exe)
    try {
        $buf = New-Object byte[] 1048576
        $done = 0L
        $last = 0L
        while (($n = $in.Read($buf, 0, $buf.Length)) -gt 0) {
            $out.Write($buf, 0, $n)
            $done += $n
            if ($done - $last -ge 4194304) { $last = $done; 'PROGRESS {0} {1}' -f $done, $total }
        }
    } finally {
        $out.Dispose()
        $in.Dispose()
    }
    'PROGRESS {0} {1}' -f $done, $total

    'STAGE verify'
    # Held open until the installer has started: nothing else can change the file between the check and the run.
    $lock = [IO.File]::Open($exe, 'Open', 'Read', 'Read')
    $sig = Get-AuthenticodeSignature $exe
    $signer = '-'
    if ($sig.SignerCertificate) { $signer = $sig.SignerCertificate.GetNameInfo('SimpleName', $false) }
    'SIGNATURE {0} {1}' -f $sig.Status, $signer
    # Two ways in, nothing else: a fully valid signature by NVIDIA Corporation, or exactly the file whose size and
    # SHA-256 Better Cam has on record for this address (the pin; empty when there is none).
    $pinSize = {SIZE}
    $pinHash = '{SHA256}'
    if ($sig.Status -eq 'Valid' -and $signer -eq 'NVIDIA Corporation') {
        'ACCEPTED signature NVIDIA Corporation'
    } elseif ($pinHash) {
        if ($lock.Length -ne $pinSize) {
            throw ('The downloaded installer is not the file Better Cam has on record (its size is {0} bytes, expected {1}), and its signature could not vouch for it ({2}, signer {3}), so it was not run.' -f $lock.Length, $pinSize, $sig.Status, $signer)
        }
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $actual = [BitConverter]::ToString($sha.ComputeHash($lock)).Replace('-', '') } finally { $sha.Dispose() }
        if ($actual -ne $pinHash) {
            throw ('The downloaded installer is not the file Better Cam has on record (its SHA-256 is {0}, expected {1}), and its signature could not vouch for it ({2}, signer {3}), so it was not run.' -f $actual, $pinHash, $sig.Status, $signer)
        }
        'ACCEPTED pin {0}' -f $actual
    } elseif ($sig.Status -ne 'Valid') {
        throw ('The downloaded installer failed the signature check ({0}, signer {1}), so it was not run.' -f $sig.Status, $signer)
    } else {
        throw ('The downloaded installer is signed by {0}, not by NVIDIA Corporation, so it was not run.' -f $signer)
    }

    'STAGE install'
    Start-Process $exe -Verb RunAs -Wait
} catch {
    $e = $_.Exception
    while ($e.InnerException) { $e = $e.InnerException }
    'ERROR ' + ($e.Message -replace '\s+', ' ')
    $failed = $true
} finally {
    if ($lock) { $lock.Dispose() }
    Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
}
if ($failed) { exit 1 }
