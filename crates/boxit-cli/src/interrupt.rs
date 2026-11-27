//! Ctrl+C.
//!
//! The first press cancels the operation at work: it stops at its next
//! checkpoint and removes what it has written, as it does after any
//! failure. A second press ends the process at once, for the case that the
//! operation does not get to a checkpoint.

use std::process;
use std::sync::{Mutex, Once, PoisonError};

use boxit_core::Control;

use crate::EXIT_CANCELLED;

/// The operation a press of Ctrl+C stops.
static RUNNING: Mutex<Option<Control>> = Mutex::new(None);

static HANDLER: Once = Once::new();

fn pressed() {
    let running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
    match running.as_ref() {
        Some(control) if !control.is_cancelled() => control.cancel(),
        _ => process::exit(EXIT_CANCELLED),
    }
}

/// Makes Ctrl+C cancel the operation of `control` from now on.
pub(crate) fn watch(control: &Control) {
    *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = Some(control.clone());
    HANDLER.call_once(|| {
        if let Err(error) = ctrlc::set_handler(pressed) {
            // Ctrl+C then ends the process the way the system does it.
            tracing::warn!("Ctrl+C cannot be handled: {error}");
        }
    });
}
