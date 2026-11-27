# Signs the Windows 11 menu package and registers it for the current user.
# The package points at BoxIt.exe and boxit_shell.dll in -External; those
# files stay where they are. A self-signed certificate is trusted only for
# this user, which is enough for the menu and does not ask for administrator.
param(
    [Parameter(Mandatory)][string]$Pack,
    [Parameter(Mandatory)][string]$External,
    [Parameter(Mandatory)][string]$Msix,
    [Parameter(Mandatory)][string]$Cer,
    [switch]$Machine
)

$ErrorActionPreference = "Stop"

function Find-Kit([string]$Name) {
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $root = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (-not (Test-Path $root)) { return $null }
    $found = Get-ChildItem $root -Recurse -Filter $Name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if ($found) { return $found.FullName }
    return $null
}

function Trust([System.Security.Cryptography.X509Certificates.X509Certificate2]$Cert) {
    Export-Certificate -Cert $Cert -FilePath $Cer | Out-Null
    # AppX builds the chain from a trusted root, then accepts the signer from
    # Trusted People. Both are the current user, so this does not need an
    # administrator.
    foreach ($store in @("Cert:\CurrentUser\Root", "Cert:\CurrentUser\TrustedPeople")) {
        $have = Get-ChildItem $store | Where-Object { $_.Thumbprint -eq $Cert.Thumbprint }
        if (-not $have) {
            Import-Certificate -FilePath $Cer -CertStoreLocation $store | Out-Null
        }
    }
}

$makeappx = Find-Kit "makeappx.exe"
$signtool = Find-Kit "signtool.exe"
$signed = $false

if ($makeappx -and $signtool) {
    $cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.FriendlyName -eq "BoxIt menu" } |
        Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type Custom -Subject "CN=BoxIt" -FriendlyName "BoxIt menu" `
            -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature -KeyExportPolicy Exportable `
            -HashAlgorithm SHA256 -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
    }
    $password = -join ((48..57) + (65..90) + (97..122) | Get-Random -Count 24 | ForEach-Object { [char]$_ })
    $secure = ConvertTo-SecureString -String $password -Force -AsPlainText
    $pfx = Join-Path $env:TEMP ("boxit-menu-" + [guid]::NewGuid().ToString() + ".pfx")
    try {
        Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $secure | Out-Null
        & $makeappx pack /o /d $Pack /nv /p $Msix
        if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE)" }
        & $signtool sign /fd SHA256 /f $pfx /p $password $Msix
        if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE)" }
    } finally {
        Remove-Item $pfx -Force -ErrorAction SilentlyContinue
    }
    Trust $cert
    $signed = $true
}

if ($Machine) {
    foreach ($store in @("Cert:\LocalMachine\Root", "Cert:\LocalMachine\TrustedPeople")) {
        Import-Certificate -FilePath $Cer -CertStoreLocation $store | Out-Null
    }
}

Get-AppxPackage -Name "BoxIt.Menu" | Remove-AppxPackage

if ($signed -or (Test-Path $Msix)) {
    if (-not (Test-Path $Cer)) { throw "The menu certificate is missing." }
    if (-not $signed) {
        $trusted = Get-ChildItem Cert:\CurrentUser\TrustedPeople
        $public = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $Cer
        if (-not ($trusted | Where-Object { $_.Thumbprint -eq $public.Thumbprint })) {
            Import-Certificate -FilePath $Cer -CertStoreLocation Cert:\CurrentUser\TrustedPeople | Out-Null
        }
    }
    try {
        Add-AppxPackage -Path $Msix -ExternalLocation $External
    } catch {
        $detail = $_ | Out-String
        if ($Machine -or ($detail -notmatch "800B0109")) { throw }
        # This Windows build only trusts the menu certificate for the whole
        # computer. The user confirms that once, in the system prompt.
        $arguments = @(
            "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $PSCommandPath,
            "-Pack", $Pack, "-External", $External, "-Msix", $Msix, "-Cer", $Cer, "-Machine"
        )
        $elevated = Start-Process -FilePath "powershell.exe" -Verb RunAs -Wait -PassThru -ArgumentList $arguments
        if ($elevated.ExitCode -ne 0) { throw "The Windows 11 menu was not registered." }
    }
    return
}

$unlock = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock" -ErrorAction SilentlyContinue
if ($unlock.AllowDevelopmentWithoutDevLicense -eq 1) {
    Add-AppxPackage -Register (Join-Path $Pack "AppxManifest.xml") -ExternalLocation $External
    return
}

throw "The Windows SDK is not installed, and Developer Mode is off, so the Windows 11 menu cannot be registered."
