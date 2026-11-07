# Downloads Microsoft's ConPTY (conpty.dll + OpenConsole.exe, MIT, from the
# Windows Terminal project) into vendor\conpty. crates\ui\build.rs copies it
# next to the exe, and scripts\package.ps1 ships it. alacritty_terminal loads
# conpty.dll from the program folder when it is there, instead of the ConPTY
# built into Windows; that one drops mouse-mode requests (?1000h ...) that a
# program writes before switching its console to raw input, so the wheel
# can't scroll such programs (Pi's fullscreen mode, for one).
# Usage: powershell -File scripts\fetch-conpty.ps1
$ErrorActionPreference = 'Stop'
$version = '1.25.260930003'
$sha256 = '02B07B349AF66D801159BDF9E440D4A1CE78BB951F37FC8609731665AFDAE7EE'
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'vendor\conpty'
$stamp = Join-Path $dest 'VERSION'
if ((Test-Path $stamp) -and (Get-Content $stamp -Raw).Trim() -eq $version -and
    (Test-Path (Join-Path $dest 'conpty.dll')) -and (Test-Path (Join-Path $dest 'x64\OpenConsole.exe'))) {
    Write-Host "ConPTY $version is already in $dest"
    return
}

$work = Join-Path ([IO.Path]::GetTempPath()) "rc-conpty-$version"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force $work | Out-Null
$pkg = Join-Path $work 'conpty.nupkg'
$url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$version/microsoft.windows.console.conpty.$version.nupkg"
Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $pkg
$actual = (Get-FileHash -Algorithm SHA256 $pkg).Hash
if ($actual -ne $sha256) { throw "ConPTY package hash mismatch: expected $sha256, got $actual" }

Add-Type -AssemblyName System.IO.Compression.FileSystem
$unpacked = Join-Path $work 'pkg'
[IO.Compression.ZipFile]::ExtractToDirectory($pkg, $unpacked)
$dll = Join-Path $unpacked 'runtimes\win-x64\native\conpty.dll'
$host64 = Join-Path $unpacked 'build\native\runtimes\x64\OpenConsole.exe'
foreach ($f in $dll, $host64) {
    $sig = Get-AuthenticodeSignature $f
    if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch '^CN=Microsoft Corporation,') {
        throw "$f is not signed by Microsoft ($($sig.Status))"
    }
}

# conpty.dll looks for OpenConsole.exe in an architecture folder next to it.
New-Item -ItemType Directory -Force (Join-Path $dest 'x64') | Out-Null
Copy-Item $dll $dest -Force
Copy-Item $host64 (Join-Path $dest 'x64') -Force
Set-Content -Path $stamp -Value $version -Encoding ASCII
Remove-Item -Recurse -Force $work
Write-Host "ConPTY $version is in $dest"
