# BoxIt and 7-Zip on the same files, on the same machine, one after the
# other: how fast each packs the sets of the benchmark corpus into ZIP and
# 7z at the usual level, how small the archives come out, and how fast each
# unpacks the archive 7-Zip made. Run by .github/workflows/bench.yml; the
# numbers of two machines are not comparable, the ratio between the two
# programs on one machine is.
#
#   pwsh scripts/bench-vs-7zip.ps1 -Cli target/release/boxit-cli.exe
#   pwsh scripts/bench-vs-7zip.ps1 -Cli ... -Small          a few megabytes, to try the script
#   pwsh scripts/bench-vs-7zip.ps1 -Cli ... -Report out.json

param(
    [Parameter(Mandatory = $true)][string]$Cli,
    [string]$Work = (Join-Path ([IO.Path]::GetTempPath()) "boxit-vs-7zip"),
    [int]$Runs = 3,
    [switch]$Small,
    [string]$Report = ""
)

$ErrorActionPreference = "Stop"

$sevenZip = @("7z", "7zz", "7za") |
    Where-Object { Get-Command $_ -ErrorAction SilentlyContinue } |
    Select-Object -First 1
if (-not $sevenZip) { throw "7-Zip was not found on the PATH" }
$Cli = (Resolve-Path $Cli).Path

# The best time of several runs: the others were slowed down by something
# else the machine was doing. $Reset puts things back before every run and
# is not timed.
function Measure-Best([scriptblock]$Action, [scriptblock]$Reset) {
    $best = [double]::MaxValue
    for ($run = 0; $run -lt $Runs; $run++) {
        & $Reset
        $seconds = (Measure-Command { & $Action }).TotalSeconds
        if ($LASTEXITCODE -ne 0) { throw "a command failed with the exit code $LASTEXITCODE" }
        if ($seconds -lt $best) { $best = $seconds }
    }
    $best
}

function Remove-Quietly([string]$Path) {
    Remove-Item -Recurse -Force $Path -ErrorAction SilentlyContinue
}

Remove-Quietly $Work
New-Item -ItemType Directory -Force $Work | Out-Null

# The same files for both programs, written by BoxIt: the corpus is
# generated, the same bytes on every machine.
$corpus = Join-Path $Work "corpus"
$scale = if ($Small) { @() } else { @("--full") }
$sets = (& $Cli bench --corpus $corpus @scale --json | Out-String) | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "the corpus was not written" }

$cases = @(
    @{ Format = "zip"; Switches = @("-tzip", "-mx=5") },
    @{ Format = "7z"; Switches = @("-t7z", "-mx=5") }
)

$rows = foreach ($set in $sets) {
    foreach ($case in $cases) {
        $ours = Join-Path $Work "boxit.$($case.Format)"
        $theirs = Join-Path $Work "7zip.$($case.Format)"
        $unpacked = Join-Path $Work "unpacked"
        $source = $set.path
        $switches = $case.Switches

        $boxitPack = Measure-Best { & $Cli create $ours $source --overwrite --quiet | Out-Null } { }
        # 7-Zip adds to an archive that exists.
        $sevenPack = Measure-Best { & $sevenZip a @switches $theirs $source | Out-Null } { Remove-Quietly $theirs }

        $clear = { Remove-Quietly $unpacked }
        $boxitUnpack = Measure-Best { & $Cli extract $theirs -o $unpacked --quiet | Out-Null } $clear
        $sevenUnpack = Measure-Best { & $sevenZip x $theirs "-o$unpacked" -y | Out-Null } $clear
        & $clear

        [pscustomobject]@{
            set                = $set.set
            format             = $case.Format
            files              = $set.files
            bytes              = $set.bytes
            boxitPacked        = (Get-Item $ours).Length
            sevenZipPacked     = (Get-Item $theirs).Length
            boxitPackSeconds   = $boxitPack
            sevenZipPackSeconds = $sevenPack
            boxitUnpackSeconds = $boxitUnpack
            sevenZipUnpackSeconds = $sevenUnpack
        }
        Remove-Quietly $ours
        Remove-Quietly $theirs
    }
}
Remove-Quietly $Work

$megabyte = 1024 * 1024
function Format-Speed([double]$Bytes, [double]$Seconds) {
    if ($Seconds -le 0) { return "-" }
    "{0:N1} MB/s" -f ($Bytes / $megabyte / $Seconds)
}
function Format-Size([double]$Bytes) {
    "{0:N1} MB" -f ($Bytes / $megabyte)
}
# How BoxIt stands against 7-Zip: above 100% it is the faster one.
function Format-Share([double]$Theirs, [double]$Ours) {
    if ($Ours -le 0) { return "-" }
    "{0:N0}%" -f ($Theirs / $Ours * 100)
}

$table = @(
    "| Set | Format | Content | BoxIt archive | 7-Zip archive | BoxIt packs | 7-Zip packs | BoxIt / 7-Zip | BoxIt unpacks | 7-Zip unpacks | BoxIt / 7-Zip |",
    "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
)
foreach ($row in $rows) {
    $table += "| " + (@(
            $row.set,
            $row.format,
            (Format-Size $row.bytes),
            (Format-Size $row.boxitPacked),
            (Format-Size $row.sevenZipPacked),
            (Format-Speed $row.bytes $row.boxitPackSeconds),
            (Format-Speed $row.bytes $row.sevenZipPackSeconds),
            (Format-Share $row.sevenZipPackSeconds $row.boxitPackSeconds),
            (Format-Speed $row.bytes $row.boxitUnpackSeconds),
            (Format-Speed $row.bytes $row.sevenZipUnpackSeconds),
            (Format-Share $row.sevenZipUnpackSeconds $row.boxitUnpackSeconds)
        ) -join " | ") + " |"
}
$table | ForEach-Object { $_ }

if ($env:GITHUB_STEP_SUMMARY) {
    @("## BoxIt and 7-Zip", "") + $table | Add-Content -Encoding utf8 $env:GITHUB_STEP_SUMMARY
}
if ($Report) {
    $rows | ConvertTo-Json | Set-Content -Encoding utf8 $Report
}
