// Without this a release build would open a console window next to the
// application window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    std::process::exit(boxit_lib::run());
}
