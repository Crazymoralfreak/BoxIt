//! Passwords in the credential store of the system.
//!
//! On Windows that is the Credential Manager. They are not written into the
//! data folder, so copying a portable folder does not copy them. The
//! settings keep the names only.

use std::path::Path;

use boxit_core::{Error, Password, Result};
use boxit_engine::passwords;
use boxit_engine::settings::Settings;
use boxit_engine::Engine;

#[cfg(windows)]
const SERVICE: &str = "BoxIt";

fn store(error: impl std::fmt::Display) -> Error {
    Error::new("passwordStore").detail(error.to_string())
}

#[cfg(windows)]
fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).map_err(store)
}

/// The password kept under `account`, when there is one.
pub fn read(account: &str) -> Result<Option<Password>> {
    #[cfg(windows)]
    {
        match entry(account)?.get_password() {
            Ok(text) => Ok(Some(Password::new(text))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(store(error)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = account;
        Err(Error::new("passwordStore"))
    }
}

/// Stores a password under `account`, replacing the one that was there.
pub fn write(account: &str, password: &Password) -> Result<()> {
    #[cfg(windows)]
    {
        entry(account)?
            .set_password(password.expose())
            .map_err(store)
    }
    #[cfg(not(windows))]
    {
        let _ = (account, password);
        Err(Error::new("passwordStore"))
    }
}

/// Removes the password under `account`. Nothing there is not an error.
pub fn delete(account: &str) -> Result<()> {
    #[cfg(windows)]
    {
        match entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(store(error)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = account;
        Ok(())
    }
}

/// Saves a named password and returns the settings with its name.
pub fn save(engine: &Engine, name: &str, password: &Password) -> Result<Settings> {
    write(&passwords::saved_account(name)?, password)?;
    let mut settings = engine.settings();
    passwords::keep_name(&mut settings.saved_passwords, name)?;
    engine.set_settings(settings)
}

/// Forgets a named password and returns the settings without its name.
pub fn forget(engine: &Engine, name: &str) -> Result<Settings> {
    if let Ok(account) = passwords::saved_account(name) {
        delete(&account)?;
    }
    let mut settings = engine.settings();
    passwords::drop_name(&mut settings.saved_passwords, name);
    engine.set_settings(settings)
}

/// The named password, when it is still in the store.
pub fn named(name: &str) -> Result<Option<Password>> {
    read(&passwords::saved_account(name)?)
}

/// Remembers the password that opened an archive.
pub fn remember_archive(path: &Path, password: &Password) -> Result<()> {
    write(&passwords::archive_account(path)?, password)
}

/// The password remembered for an archive.
pub fn recall_archive(path: &Path) -> Result<Option<Password>> {
    read(&passwords::archive_account(path)?)
}

/// Forgets the password remembered for an archive.
pub fn forget_archive(path: &Path) -> Result<()> {
    delete(&passwords::archive_account(path)?)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips_through_the_credential_store() {
        let account = format!("test:{}", boxit_engine::fsx::unique_suffix());
        let password = Password::from("open sesame");
        write(&account, &password).unwrap();
        let read_back = read(&account).unwrap().unwrap();
        assert_eq!(read_back.expose(), password.expose());
        delete(&account).unwrap();
        assert!(read(&account).unwrap().is_none());
        // A second removal still succeeds.
        delete(&account).unwrap();
    }
}
