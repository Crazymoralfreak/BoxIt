//! The border of an operation: a panic inside stops the operation, not the
//! program.
//!
//! Archives are data from strangers and the libraries that read them are
//! large. A damaged file that trips an assertion somewhere deep must end as
//! an error message, with the temporary files cleaned up by the destructors
//! that run while the panic unwinds.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

use boxit_core::{Error, Result};

fn message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic")
}

/// Runs an operation; a panic inside comes back as the `internal` error.
pub fn guarded<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    // The state an operation leaves behind after a panic is dropped by its
    // owner and never looked at again, which is what unwind safety asks for.
    match panic::catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(payload) => {
            let text = message(payload.as_ref());
            tracing::error!("an operation panicked: {text}");
            Err(Error::new("internal").detail(text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_pass_through() {
        assert_eq!(guarded(|| Ok(5)), Ok(5));
        assert_eq!(
            guarded(|| Err::<(), _>(Error::new("io"))),
            Err(Error::new("io"))
        );
    }

    #[test]
    fn a_panic_becomes_an_error_and_destructors_run() {
        struct Flag<'a>(&'a mut bool);
        impl Drop for Flag<'_> {
            fn drop(&mut self) {
                *self.0 = true;
            }
        }
        let mut dropped = false;
        let result: Result<()> = guarded(|| {
            let _flag = Flag(&mut dropped);
            panic!("broken {}", 7);
        });
        let error = result.unwrap_err();
        assert_eq!(error.code, "internal");
        assert_eq!(error.params["detail"], "broken 7");
        assert!(dropped);
    }
}
