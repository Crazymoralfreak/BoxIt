//! `boxit-cli`: the command line of BoxIt as a program of its own, for the
//! systems and the scripts that have no use for the window.

use std::env;
use std::io::{self, IsTerminal};
use std::process;

use boxit_cli::Host;
use boxit_engine::{logging, stored_language, Paths};

fn main() {
    let paths = Paths::detect();
    let code = {
        // Flushes the log when the command is done, before the process ends.
        let _log = logging::init(&paths.logs(), false);
        let host = Host {
            language: stored_language(&paths),
            prompts: io::stdin().is_terminal() && io::stderr().is_terminal(),
        };
        boxit_cli::run(env::args_os(), &host)
    };
    process::exit(code);
}
