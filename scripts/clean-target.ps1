# Trims the cargo build cache in target\ (it grows without bound).
#
#   powershell -File scripts\clean-target.ps1            # remove what wasn't used in 7 days
#   powershell -File scripts\clean-target.ps1 -Days 3
#   powershell -File scripts\clean-target.ps1 -All       # remove all incremental caches
#   powershell -File scripts\clean-target.ps1 -WhatIf    # only show what would go
#
# What it removes:
#   - target\<profile>\incremental\* folders not written for -Days days (or all with -All).
#     These are only a compile cache; the next build of the workspace crates is a bit slower.
#   - stale artifacts in target\<profile>\deps etc. via `cargo sweep --time <Days>`, if
#     cargo-sweep is installed (`cargo install cargo-sweep`); skipped otherwise.
# dist\ and the final exe are never touched. Refuses to run while cargo/rustc is running.

param(
    [int]$Days = 7,
    [switch]$All,
    [switch]$WhatIf
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$target = Join-Path $root 'target'
if (-not (Test-Path $target)) { Write-Host 'No target folder, nothing to do.'; exit 0 }

if (Get-Process cargo, rustc, cargo-clippy, clippy-driver -ErrorAction SilentlyContinue) {
    Write-Error 'cargo/rustc is running; stop the build first.'
}

function Get-SizeGB([string]$path) {
    $sum = (Get-ChildItem $path -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
    [math]::Round(($sum + 0) / 1GB, 2)
}

$before = Get-SizeGB $target
$cutoff = (Get-Date).AddDays(-$Days)
$removed = 0

# Incremental caches: one folder per crate and build kind (build/test/clippy/features).
foreach ($inc in Get-ChildItem $target -Directory | ForEach-Object { Join-Path $_.FullName 'incremental' }) {
    if (-not (Test-Path $inc)) { continue }
    foreach ($dir in Get-ChildItem $inc -Directory) {
        # A folder's own timestamp can lag behind its session subfolders; use the newest.
        $newest = (@($dir) + @(Get-ChildItem $dir.FullName -Directory)) |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($All -or $newest.LastWriteTime -lt $cutoff) {
            if ($WhatIf) { Write-Host "would remove $($dir.FullName)" }
            else { Remove-Item -Recurse -Force $dir.FullName }
            $removed++
        }
    }
}
Write-Host "Incremental folders $(if ($WhatIf) { 'to remove' } else { 'removed' }): $removed"

# Stale deps/build artifacts.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path (Join-Path $cargoBin 'cargo-sweep.exe')) {
    $env:Path = "$cargoBin;$env:Path"
    $sweepArgs = @('sweep', '--time', $Days)
    if ($WhatIf) { $sweepArgs += '--dry-run' }
    Push-Location $root
    try { & cargo @sweepArgs } finally { Pop-Location }
} else {
    Write-Host 'cargo-sweep not installed; skipping deps cleanup (cargo install cargo-sweep).'
}

if (-not $WhatIf) {
    $after = Get-SizeGB $target
    Write-Host "target: $before GB -> $after GB"
}
