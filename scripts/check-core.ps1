# The local check of the archive crates: formatting, tests and lints, with
# only the lines that matter printed. The crate of the window (src-tauri) is
# checked as well when asked for; the application itself is not built here.
#
#   pwsh scripts/check-core.ps1            the archive crates
#   pwsh scripts/check-core.ps1 -Window    and the crate of the window
#   pwsh scripts/check-core.ps1 -NoTests   formatting and lints only

param([switch]$NoTests, [switch]$Window)

$packages = @(Get-ChildItem (Join-Path $PSScriptRoot "..\crates") -Directory |
    Where-Object { Test-Path (Join-Path $_.FullName "Cargo.toml") } |
    ForEach-Object { "-p"; $_.Name })
if ($Window) { $packages += @("-p", "boxit") }

cargo fmt --all

$failed = $false
if (-not $NoTests) {
    $output = cargo test @packages 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE -ne 0) { $failed = $true }
    $passed = 0
    foreach ($line in $output) {
        if ($line -match "test result: \w+\. (\d+) passed") { $passed += [int]$Matches[1] }
    }
    $output | Select-String -CaseSensitive -Pattern "FAILED|panicked|^error|^warning: unused|assertion|left:|right:|-->" -Context 0, 6 |
        Select-Object -First 40
    "tests passed: $passed"
}

$output = cargo clippy @packages --all-targets -- -D warnings 2>&1 | ForEach-Object { "$_" }
if ($LASTEXITCODE -ne 0) { $failed = $true }
$output | Select-String -Pattern "^(warning|error)" -Context 0, 14 | Select-Object -First 12
if ($failed) { "FAILED"; exit 1 }
if ($Window) { "core and window are clean" } else { "core is clean" }
