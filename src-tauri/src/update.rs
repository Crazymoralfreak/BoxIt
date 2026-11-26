//! Updates from the GitHub releases of BoxIt. This is the only place where
//! BoxIt reaches for the network, and it does so only when the user presses
//! "Check for updates": nothing is asked at startup or in the background.
//!
//! A check only tells that a newer version exists; nothing is downloaded
//! until the user agrees. A copy put in place by the installer then
//! downloads the new installer, checks its signature against the key in
//! `tauri.conf.json` and runs it once no operation is running; the
//! installer starts BoxIt again. A portable copy cannot replace itself, so
//! it only tells that a newer version exists.

use std::env;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;
use tauri_plugin_updater::{Update, UpdaterExt};

use boxit_core::{Error, Result};

use crate::app::{lock, Core};

const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(15);
/// How often a downloaded update looks whether BoxIt has become idle.
const IDLE_POLL: Duration = Duration::from_secs(1);
/// The installer leaves this file next to the program; a portable copy has none.
const UNINSTALLER: &str = "uninstall.exe";
/// Where the releases are described for a reader.
const RELEASES: &str = "https://github.com/Crazymoralfreak/BoxIt/releases";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateState {
    /// Nothing was asked in this session.
    Idle,
    Checking,
    UpToDate,
    /// A newer version exists; the user decides whether to install it.
    Available,
    Downloading,
    /// Downloaded and checked; waits for the running operations to finish.
    Ready,
    Installing,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub state: UpdateState,
    /// The version found; with `Failed`, the version that could not be
    /// installed, and nothing when the check itself failed.
    pub version: Option<String>,
    /// Downloaded and total bytes of the installer; the total is zero
    /// while it is not known.
    pub downloaded: u64,
    pub total: u64,
    /// This copy can replace itself with a newer one.
    pub installable: bool,
}

/// True for a copy that was put in place by the installer.
fn installed_copy() -> bool {
    env::current_exe()
        .ok()
        .and_then(|program| {
            program
                .parent()
                .map(|folder| folder.join(UNINSTALLER).is_file())
        })
        .unwrap_or(false)
}

/// What is known about updates in this session.
pub struct Tracker {
    status: Mutex<UpdateStatus>,
    /// One check or installation at a time.
    busy: AtomicBool,
    /// The version the last check found, kept until the user decides about it.
    found: Mutex<Option<Update>>,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            status: Mutex::new(UpdateStatus {
                state: UpdateState::Idle,
                version: None,
                downloaded: 0,
                total: 0,
                installable: installed_copy(),
            }),
            busy: AtomicBool::new(false),
            found: Mutex::new(None),
        }
    }

    pub fn status(&self) -> UpdateStatus {
        lock(&self.status).clone()
    }

    fn change(&self, change: impl FnOnce(&mut UpdateStatus)) -> UpdateStatus {
        let mut status = lock(&self.status);
        change(&mut status);
        status.clone()
    }

    fn keep(&self, update: Option<Update>) {
        *lock(&self.found) = update;
    }

    fn found(&self) -> Option<Update> {
        lock(&self.found).clone()
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

/// The check asked for by the user. The result arrives as `update:status`
/// events.
pub fn check_now(core: &Arc<Core>) {
    let core = Arc::clone(core);
    spawn(move || alone(&core, check));
}

/// The user agreed to the version the last check found: it is downloaded
/// and installed in the background. The progress arrives as
/// `update:status` events.
pub fn install_now(core: &Arc<Core>) -> Result<()> {
    if !core.updates.status().installable {
        return Err(Error::new("updatePortable"));
    }
    let core = Arc::clone(core);
    spawn(move || alone(&core, install));
    Ok(())
}

fn spawn(work: impl FnOnce() + Send + 'static) {
    if let Err(error) = thread::Builder::new()
        .name(String::from("boxit-update"))
        .spawn(work)
    {
        tracing::warn!("the update thread did not start: {error}");
    }
}

/// Runs one piece of work with updates at a time; a second request is dropped.
fn alone(core: &Arc<Core>, work: fn(&Arc<Core>)) {
    if core.updates.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    work(core);
    core.updates.busy.store(false, Ordering::SeqCst);
}

fn tell(core: &Core, status: &UpdateStatus) {
    let _ = core.app().emit("update:status", status);
}

fn set(core: &Core, state: UpdateState, version: Option<&str>) {
    let status = core.updates.change(|status| {
        status.state = state;
        status.version = version.map(str::to_owned);
    });
    tell(core, &status);
}

fn find(core: &Core) -> tauri_plugin_updater::Result<Option<Update>> {
    let updater = core
        .app()
        .updater_builder()
        .timeout(CHECK_TIMEOUT)
        .build()?;
    tauri::async_runtime::block_on(updater.check())
}

/// Looks for a newer version and tells about it. Nothing is downloaded here.
fn check(core: &Arc<Core>) {
    set(core, UpdateState::Checking, None);
    match find(core) {
        Ok(Some(update)) => {
            let version = update.version.clone();
            tracing::info!(%version, "an update was found");
            core.updates.keep(Some(update));
            set(core, UpdateState::Available, Some(&version));
        }
        Ok(None) => {
            core.updates.keep(None);
            set(core, UpdateState::UpToDate, None);
        }
        Err(error) => {
            tracing::warn!("the check for updates failed: {error}");
            set(core, UpdateState::Failed, None);
        }
    }
}

/// Downloads the version the user agreed to and hands over to its installer.
fn install(core: &Arc<Core>) {
    let Some(mut update) = core.updates.found() else {
        // Nothing was found to agree to: the button is not there without a check.
        set(core, UpdateState::Failed, None);
        return;
    };
    let version = update.version.clone();

    core.updates.change(|status| {
        status.downloaded = 0;
        status.total = 0;
    });
    set(core, UpdateState::Downloading, Some(&version));
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    let progress = |chunk: usize, total: Option<u64>| {
        let status = core.updates.change(|status| {
            status.downloaded += chunk as u64;
            status.total = total.unwrap_or(0);
        });
        tell(core, &status);
    };
    // The signature of the downloaded installer is checked here, before
    // anything is run.
    let bytes = match tauri::async_runtime::block_on(update.download(progress, || {})) {
        Ok(bytes) => bytes,
        Err(error) => {
            failed(core, &version, &error.to_string());
            return;
        }
    };

    set(core, UpdateState::Ready, Some(&version));
    // An operation is never cut short by an update, and none starts once
    // the installer is on its way.
    while !core.begin_install() {
        thread::sleep(IDLE_POLL);
    }

    set(core, UpdateState::Installing, Some(&version));
    tracing::info!(%version, "the update is being installed");
    match update.install(&bytes) {
        // On Windows the installer has taken over and the process has ended
        // by now. Elsewhere the new copy is in place and takes over here.
        Ok(()) => core.app().restart(),
        Err(error) => {
            core.cancel_install();
            failed(core, &version, &error.to_string());
        }
    }
}

/// The version stays known, so the user can ask for it once more.
fn failed(core: &Core, version: &str, message: &str) {
    tracing::warn!(version, "the update failed: {message}");
    set(core, UpdateState::Failed, Some(version));
}

/// The page of a release on GitHub: of `version` when it is written the
/// way versions are, of the latest release otherwise. The version comes
/// from the network, so nothing but a version gets into the address.
fn release_page(version: Option<&str>) -> String {
    let plain = |version: &&str| {
        !version.is_empty()
            && version.len() <= 64
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    };
    match version.filter(plain) {
        Some(version) => format!("{RELEASES}/tag/v{version}"),
        None => format!("{RELEASES}/latest"),
    }
}

/// The program of the system that opens an address in the browser.
fn browser(address: &str) -> Command {
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(windows, target_os = "macos")))]
    let program = "xdg-open";
    let mut command = Command::new(program);
    command.arg(address);
    command
}

/// Opens the page of the version found, or of the latest release, in the
/// browser of the system.
pub fn open_page(core: &Core) -> Result<()> {
    let address = release_page(core.updates.status().version.as_deref());
    browser(&address)
        .spawn()
        .map(|_| ())
        .map_err(|error| Error::from(error).with("path", address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_version_gets_into_the_address_of_a_release() {
        assert_eq!(
            release_page(Some("0.1.0")),
            "https://github.com/Crazymoralfreak/BoxIt/releases/tag/v0.1.0"
        );
        assert_eq!(
            release_page(Some("1.2.3-beta.1+build")),
            "https://github.com/Crazymoralfreak/BoxIt/releases/tag/v1.2.3-beta.1+build"
        );
        let latest = "https://github.com/Crazymoralfreak/BoxIt/releases/latest";
        assert_eq!(release_page(None), latest);
        assert_eq!(release_page(Some("")), latest);
        assert_eq!(release_page(Some("1.0/../../evil")), latest);
        assert_eq!(release_page(Some("1.0 & calc")), latest);
        assert_eq!(release_page(Some("1.0?x=1")), latest);
        assert_eq!(release_page(Some(&"1".repeat(65))), latest);
    }

    #[test]
    fn a_copy_without_the_uninstaller_does_not_replace_itself() {
        // The tests are not run from a folder the installer filled.
        assert!(!installed_copy());
        assert!(!Tracker::new().status().installable);
        assert_eq!(Tracker::new().status().state, UpdateState::Idle);
    }
}
