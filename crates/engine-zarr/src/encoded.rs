//! Encoded-buffer admission for one synchronous native retrieval.
//!
//! zarrs converts storage Bytes into owned codec Vecs, so a guard attached only
//! to Bytes would release too early. Keep reservations until retrieval returns.
//! Repeated plain whole-object lookups share an allowance within this scope;
//! range operations accumulate conservatively. Each Icechunk chunk job gets a
//! separate scope, bounding retention to its decode rather than the whole map.
use std::{
    cell::RefCell,
    collections::HashMap,
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, Mutex},
};

use ds_core::error::DataServerError;

use crate::read_budget::{Budget, Permit};

thread_local! {
    static CURRENT: RefCell<Option<Arc<Context>>> = const { RefCell::new(None) };
}

pub(crate) struct Context {
    pub(crate) budget: Arc<Budget>,
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    objects: HashMap<String, u64>,
    permits: Vec<Arc<Permit>>,
}

impl Context {
    /// Intermediate compressed representations can exceed the native chunk
    /// estimate (nested compression or an outer-compressed shard). Admit their
    /// growing output separately, including a reallocation/copy allowance.
    pub(crate) fn intermediate(&self, additional: usize) -> Result<(), DataServerError> {
        let bytes = (additional as u64)
            .checked_mul(2)
            .ok_or(DataServerError::ResourceExhausted)?;
        self.held
            .lock()
            .unwrap()
            .permits
            .push(self.budget.reserve_bytes(bytes)?);
        Ok(())
    }

    /// Two copies cover the collected body and zarrs' owned encoded buffer.
    /// Transport buffers and codec-private allocations are separate estimates.
    pub(crate) fn object(&self, key: &str, size: u64) -> Result<(), DataServerError> {
        let mut held = self.held.lock().unwrap();
        let previous = held.objects.get(key).copied().unwrap_or(0);
        if size > previous {
            let bytes = size
                .checked_sub(previous)
                .and_then(|n| n.checked_mul(2))
                .ok_or(DataServerError::ResourceExhausted)?;
            held.permits.push(self.budget.reserve_bytes(bytes)?);
            held.objects.insert(key.to_owned(), size);
        }
        Ok(())
    }

    #[cfg(feature = "icechunk")]
    pub(crate) fn ranges(&self, size: u64) -> Result<(), DataServerError> {
        let bytes = size
            .checked_mul(2)
            .ok_or(DataServerError::ResourceExhausted)?;
        self.held
            .lock()
            .unwrap()
            .permits
            .push(self.budget.reserve_bytes(bytes)?);
        Ok(())
    }
}

pub(crate) fn current() -> Option<Arc<Context>> {
    CURRENT.with_borrow(Clone::clone)
}

pub(crate) fn enter(budget: Option<Arc<Budget>>) -> Guard {
    let context = budget.map(|budget| {
        Arc::new(Context {
            budget,
            held: Mutex::default(),
        })
    });
    Guard {
        previous: CURRENT.replace(context),
        _thread: PhantomData,
    }
}

pub(crate) struct Guard {
    previous: Option<Arc<Context>>,
    _thread: PhantomData<Rc<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.set(self.previous.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_scopes_restore_context_and_unwinding_releases_allocations() {
        let budget = Arc::new(Budget::new(100));
        {
            let _outer = enter(Some(budget.clone()));
            current().unwrap().object("a", 10).unwrap();
            let result = std::panic::catch_unwind(|| {
                let _inner = enter(Some(budget.clone()));
                current().unwrap().object("b", 20).unwrap();
                assert_eq!(budget.metrics().0, 60);
                panic!("codec failure");
            });
            assert!(result.is_err());
            assert_eq!(budget.metrics().0, 20);
            current().unwrap().object("a", 15).unwrap();
            assert_eq!(
                budget.metrics().0,
                30,
                "growing objects reserve the additional bytes"
            );
            assert!(matches!(
                current().unwrap().object("c", 36),
                Err(DataServerError::ResourceExhausted)
            ));
            assert_eq!(budget.metrics().0, 30);
        }
        assert!(current().is_none());
        assert_eq!(budget.metrics().0, 0);
    }
}
