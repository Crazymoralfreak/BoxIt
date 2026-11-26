//! The Windows 11 menu. A small package gives BoxIt an identity, and
//! Explorer then loads `boxit_shell.dll` from the folder of BoxIt.exe.
//! The certificate is created for the current user; nothing is written
//! for the whole machine.

use std::fmt::Write;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use boxit_core::{Error, Result};

const CLASS: &str = "A7C3E4B2-1D58-4F0A-9B6E-3C8D5F1A2E70";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn failed(detail: impl Into<String>) -> Error {
    Error::new("shellFailed").detail(detail.into())
}

fn manifest() -> String {
    let mut items = String::new();
    for kind in ["*", "Directory"] {
        let _ = write!(
            items,
            "            <desktop5:ItemType Type=\"{kind}\">\n              <desktop5:Verb Id=\"BoxIt\" Clsid=\"{CLASS}\" />\n            </desktop5:ItemType>\n"
        );
    }
    let version = concat!(env!("CARGO_PKG_VERSION"), ".0");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Package
  xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:uap10="http://schemas.microsoft.com/appx/manifest/uap/windows10/10"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  xmlns:com="http://schemas.microsoft.com/appx/manifest/com/windows10"
  xmlns:desktop4="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4"
  xmlns:desktop5="http://schemas.microsoft.com/appx/manifest/desktop/windows10/5"
  IgnorableNamespaces="uap uap10 rescap com desktop4 desktop5">
  <Identity Name="BoxIt.Menu" Publisher="CN=BoxIt" Version="{version}" ProcessorArchitecture="neutral" />
  <Properties>
    <DisplayName>BoxIt</DisplayName>
    <PublisherDisplayName>BoxIt</PublisherDisplayName>
    <Logo>Assets\StoreLogo.png</Logo>
    <uap10:AllowExternalContent>true</uap10:AllowExternalContent>
  </Properties>
  <Resources>
    <Resource Language="en-us" />
  </Resources>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.22000.0" MaxVersionTested="10.0.26100.0" />
  </Dependencies>
  <Capabilities>
    <rescap:Capability Name="runFullTrust" />
    <rescap:Capability Name="unvirtualizedResources" />
  </Capabilities>
  <Applications>
    <Application Id="BoxIt" Executable="BoxIt.exe" uap10:TrustLevel="mediumIL" uap10:RuntimeBehavior="win32App">
      <uap:VisualElements AppListEntry="none" DisplayName="BoxIt" Description="BoxIt" BackgroundColor="transparent" Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png" />
      <Extensions>
        <com:Extension Category="windows.comServer">
          <com:ComServer>
            <com:SurrogateServer DisplayName="BoxIt">
              <com:Class Id="{CLASS}" Path="boxit_shell.dll" ThreadingModel="STA" />
            </com:SurrogateServer>
          </com:ComServer>
        </com:Extension>
        <desktop4:Extension Category="windows.fileExplorerContextMenus">
          <desktop4:FileExplorerContextMenus>
{items}          </desktop4:FileExplorerContextMenus>
        </desktop4:Extension>
      </Extensions>
    </Application>
  </Applications>
</Package>
"#
    )
}

fn write_pack(dir: &Path) -> Result<()> {
    let assets = dir.join("Assets");
    fs::create_dir_all(&assets).map_err(|error| failed(error.to_string()))?;
    fs::write(
        assets.join("StoreLogo.png"),
        include_bytes!("../icons/128x128.png"),
    )
    .map_err(|error| failed(error.to_string()))?;
    fs::write(
        assets.join("Square150x150Logo.png"),
        include_bytes!("../icons/128x128.png"),
    )
    .map_err(|error| failed(error.to_string()))?;
    fs::write(
        assets.join("Square44x44Logo.png"),
        include_bytes!("../icons/32x32.png"),
    )
    .map_err(|error| failed(error.to_string()))?;
    fs::write(dir.join("AppxManifest.xml"), manifest())
        .map_err(|error| failed(error.to_string()))?;
    fs::write(
        dir.join("pack-menu.ps1"),
        include_str!("../../scripts/pack-menu.ps1"),
    )
    .map_err(|error| failed(error.to_string()))
}

fn powershell(script: &Path, arguments: &[PathBuf]) -> Result<()> {
    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(script);
    for argument in arguments {
        command.arg(argument);
    }
    let output = command
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| failed(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = if detail.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        detail.into_owned()
    };
    let detail: String = detail.chars().take(800).collect();
    Err(failed(detail))
}

/// Registers the Windows 11 menu when `boxit_shell.dll` sits beside the program.
/// Without that library the classic menu is unchanged.
pub fn install(program: &Path) -> Result<()> {
    let Some(folder) = program.parent() else {
        return Err(failed("the program has no folder"));
    };
    if !folder.join("boxit_shell.dll").is_file() {
        return Ok(());
    }
    let pack = std::env::temp_dir().join(format!("boxit-menu-{}", std::process::id()));
    let _ = fs::remove_dir_all(&pack);
    write_pack(&pack)?;
    let outcome = powershell(
        &pack.join("pack-menu.ps1"),
        &[
            PathBuf::from("-Pack"),
            pack.clone(),
            PathBuf::from("-External"),
            folder.to_path_buf(),
            PathBuf::from("-Msix"),
            folder.join("BoxIt.Menu.msix"),
            PathBuf::from("-Cer"),
            folder.join("BoxIt.Menu.cer"),
        ],
    );
    let _ = fs::remove_dir_all(&pack);
    outcome
}

/// Removes the Windows 11 menu package. A machine that never had it is fine.
pub fn remove() -> Result<()> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-AppxPackage -Name 'BoxIt.Menu' | Remove-AppxPackage",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| failed(error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failed(String::from_utf8_lossy(&output.stderr).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_the_menu_library() {
        let text = manifest();
        assert!(text.contains(CLASS));
        assert!(text.contains("boxit_shell.dll"));
        assert!(text.contains("Type=\"*\""));
        assert!(text.contains("Type=\"Directory\""));
    }
}
