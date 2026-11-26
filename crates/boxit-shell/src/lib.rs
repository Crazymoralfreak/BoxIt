//! The menu Explorer shows on Windows 11. Explorer loads this library into
//! its own process, so the library only starts BoxIt. It does not open
//! archives.

mod text;

#[cfg(windows)]
mod menu;
