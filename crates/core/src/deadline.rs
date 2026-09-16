//! Absolute request deadline carried through synchronous engine calls.
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Instant;

use crate::error::DataServerError;

thread_local! { static CURRENT: Cell<Option<Instant>> = const { Cell::new(None) }; }

pub fn current() -> Option<Instant> {
    CURRENT.get()
}

/// Install on each worker thread (including Rayon fan-out), restoring the prior
/// scope on return or unwind. The guard must never move to another thread.
pub fn enter(deadline: Option<Instant>) -> Guard {
    let previous = CURRENT.replace(deadline);
    Guard {
        previous,
        _thread: PhantomData,
    }
}

pub fn check() -> Result<(), DataServerError> {
    if current().is_some_and(|d| Instant::now() >= d) {
        Err(DataServerError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

pub struct Guard {
    previous: Option<Instant>,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.set(self.previous);
    }
}
