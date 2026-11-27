//! Cancelling and pausing an operation from another thread.
//!
//! The code that does the work calls [`Control::checkpoint`] between blocks
//! of data: it returns at once while the operation runs, waits while it is
//! paused and fails with the `cancelled` error once it is cancelled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use crate::error::{Error, Result};

#[derive(Default)]
struct State {
    cancelled: AtomicBool,
    paused: AtomicBool,
    /// Guards the wait of a paused operation; the flags above are the truth.
    lock: Mutex<()>,
    changed: Condvar,
}

/// A handle shared by an operation and whoever may stop it. Clones refer to
/// the same operation.
#[derive(Clone, Default)]
pub struct Control {
    state: Arc<State>,
}

impl Control {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the operation to stop. A paused operation wakes up to stop.
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.wake();
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub fn set_paused(&self, paused: bool) {
        self.state.paused.store(paused, Ordering::Release);
        if !paused {
            self.wake();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.state.paused.load(Ordering::Acquire)
    }

    fn wake(&self) {
        // Taking the lock orders the change of a flag against a waiter that
        // has checked the flags and is about to sleep.
        let _guard = self
            .state
            .lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.state.changed.notify_all();
    }

    /// Returns at once while the operation runs, waits while it is paused,
    /// fails once it is cancelled. Cheap enough to call per block of data.
    pub fn checkpoint(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(Error::cancelled());
        }
        if !self.is_paused() {
            return Ok(());
        }
        let mut guard = self
            .state
            .lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while self.is_paused() && !self.is_cancelled() {
            guard = self
                .state
                .changed
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(guard);
        if self.is_cancelled() {
            Err(Error::cancelled())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_running_operation_passes_and_a_cancelled_one_fails() {
        let control = Control::new();
        assert!(control.checkpoint().is_ok());
        control.clone().cancel();
        assert!(control.checkpoint().unwrap_err().is_cancelled());
    }

    #[test]
    fn a_paused_operation_waits_until_it_is_resumed() {
        let control = Control::new();
        control.set_paused(true);
        let (sender, receiver) = mpsc::channel();
        let worker = {
            let control = control.clone();
            thread::spawn(move || {
                let result = control.checkpoint();
                sender.send(()).unwrap();
                result
            })
        };
        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "the operation did not wait"
        );
        control.set_paused(false);
        receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn cancelling_wakes_a_paused_operation() {
        let control = Control::new();
        control.set_paused(true);
        let worker = {
            let control = control.clone();
            thread::spawn(move || control.checkpoint())
        };
        thread::sleep(Duration::from_millis(50));
        control.cancel();
        assert!(worker.join().unwrap().unwrap_err().is_cancelled());
    }
}
