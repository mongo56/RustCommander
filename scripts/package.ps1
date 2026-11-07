# Builds the release exe and packs a portable zip into dist/.
# Usage: powershell -File scripts\package.ps1
$ErrorActionPreference = 'Stop'
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Microsoft's ConPTY for the built-in terminal (vendor\conpty), shipped next to the exe.
& (Join-Path $PSScriptRoot 'fetch-conpty.ps1')

cargo build --release -p rustcommander
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$version = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$name = "RustCommander-$version-win64"
# Stage outside dist/ so an Explorer window open there can't block the rebuild.
$stage = Join-Path ([IO.Path]::GetTempPath()) "rc-package\$name"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force (Join-Path $root 'dist') | Out-Null
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item 'target\release\rustcommander.exe' $stage
Copy-Item 'vendor\conpty\conpty.dll' $stage
New-Item -ItemType Directory -Force (Join-Path $stage 'x64') | Out-Null
Copy-Item 'vendor\conpty\x64\OpenConsole.exe' (Join-Path $stage 'x64')
foreach ($f in 'LICENSE', 'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD-PARTY-NOTICES.txt') {
    if (Test-Path $f) { Copy-Item $f $stage }
}
# README without images (the screenshots aren't shipped): drop image lines
# and the Screenshots section.
$out = New-Object System.Collections.Generic.List[string]
$skip = $false
foreach ($line in Get-Content 'README.md' -Encoding UTF8) {
    if ($line -match '^## ') { $skip = $line -eq '## Screenshots' }
    if ($skip -or $line -match '!\[') { continue }
    $out.Add($line)
}
$text = ($out -join "`n") -replace "`n{3,}", "`n`n"
[IO.File]::WriteAllText((Join-Path $stage 'README.md'), $text + "`n")
$zip = Join-Path $root "dist\$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip
Write-Host ("Created {0} ({1:N1} MB)" -f $zip, ((Get-Item $zip).Length / 1MB))

# Installer (Inno Setup 6, installer\rustcommander.iss), next to the zip.
$iscc = @(
    (Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue | ForEach-Object Source),
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) {
    Write-Host 'Skipped the installer: Inno Setup 6 (ISCC.exe) is not installed. Get it with: winget install JRSoftware.InnoSetup'
    return
}
# The .ico that crates\ui\build.rs writes next to the exe's icon resource.
$ico = Get-ChildItem 'target\release\build\rustcommander-*\out\icon.ico' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
$isccArgs = @('/Q', "/DAppVersion=$version", "/DSourceDir=$stage", "/DOutputDir=$(Join-Path $root 'dist')")
if ($ico) { $isccArgs += "/DIconFile=$($ico.FullName)" }
& $iscc @isccArgs 'installer\rustcommander.iss'
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup failed' }
$setup = Join-Path $root "dist\RustCommander-$version-setup.exe"
Write-Host ("Created {0} ({1:N1} MB)" -f $setup, ((Get-Item $setup).Length / 1MB))
