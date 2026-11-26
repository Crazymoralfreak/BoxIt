//! Runs the archive glued after this program.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use boxit_core::{Error, Interaction, OpContext, Password, PasswordRequest};
use boxit_engine::text::{self, system_language};
use boxit_sfx::{invocation, unpack};

#[cfg(windows)]
mod gui;

fn main() -> ExitCode {
    ExitCode::from(run())
}

fn run() -> u8 {
    let language = system_language();
    let mut args = env::args_os();
    let _program = args.next();
    let invocation = match invocation(args) {
        Ok(invocation) => invocation,
        Err(error) => {
            tell(language, &error);
            return 2;
        }
    };
    let program = env::current_exe().unwrap_or_else(|_| PathBuf::from("boxit-sfx.exe"));
    let destination = match &invocation.output {
        Some(path) => path.clone(),
        None if invocation.yes => env::current_dir().unwrap_or_default(),
        None => match ask_folder(language) {
            Some(path) => path,
            None => return 0,
        },
    };
    let mut ctx = OpContext::silent();
    ctx.interaction = Arc::new(Answers {
        password: Mutex::new(invocation.password.clone()),
    });
    #[cfg(windows)]
    if !invocation.yes {
        ctx.progress = Arc::new(gui::Bar::start(language, ctx.control.clone()));
    }
    match unpack(
        &program,
        &destination,
        invocation.password,
        invocation.yes,
        &ctx,
    ) {
        Ok(_) => 0,
        Err(error) => {
            tell(language, &error);
            if error.is_cancelled() {
                130
            } else {
                1
            }
        }
    }
}

/// Gives the password from the command line the first time, and asks in a
/// dialog when there was none or it did not fit.
struct Answers {
    password: Mutex<Option<Password>>,
}

impl Interaction for Answers {
    fn password(&self, request: &PasswordRequest) -> Option<Password> {
        let given = self
            .password
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if given.is_some() {
            return given;
        }
        ask_password(&request.archive)
    }
}

fn tell(language: &str, error: &Error) {
    let message = text::error_text(language, error);
    #[cfg(windows)]
    gui::tell(&message);
    #[cfg(not(windows))]
    {
        eprintln!("{message}");
    }
}

fn ask_folder(language: &str) -> Option<PathBuf> {
    let title = text::text(language, "sfx.folder", &[]);
    #[cfg(windows)]
    {
        gui::folder(&title)
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        None
    }
}

fn ask_password(name: &str) -> Option<Password> {
    #[cfg(windows)]
    {
        gui::password(name).map(Password::new)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        None
    }
}
