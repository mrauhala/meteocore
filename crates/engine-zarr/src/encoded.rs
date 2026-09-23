//! Encoded-buffer admission for one synchronous native retrieval.
//!
//! zarrs converts storage Bytes into owned codec Vecs, so a guard attached only
//! to Bytes would release too early. Keep reservations until retrieval returns.
//! Repeated plain whole-object lookups share an allowance within this scope;
//! range operations accumulate conservatively. Each Icechunk chunk job gets a
//! separate scope, bounding retention to its decode rather than the whole map.
//! Cold chunk admission may prepay encoded/codec headroom. Consume that credit
//! before growing the scope's reservation; it belongs to exactly one scope.
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
    // This is a credit inside an already acquired chunk reservation. Holding
    // its owner makes propagation to storage futures safe after caller exit.
    _prepaid: Option<Arc<Permit>>,
}

#[derive(Default)]
struct Held {
    objects: HashMap<String, u64>,
    permits: Vec<Arc<Permit>>,
    credit: u64,
}

impl Held {
    fn reserve(&mut self, budget: &Arc<Budget>, bytes: u64) -> Result<(), DataServerError> {
        ds_core::deadline::check()?;
        let prepaid = self.credit.min(bytes);
        if bytes > prepaid {
            self.permits.push(budget.reserve_bytes(bytes - prepaid)?);
        }
        self.credit -= prepaid;
        Ok(())
    }
}

impl Context {
    /// Blosc's serial/getitem scratch scales with the validated block size.
    /// Retain its allowance through retrieval alongside codec-owned copies.
    pub(crate) fn codec_scratch(&self, bytes: u64) -> Result<(), DataServerError> {
        self.held.lock().unwrap().reserve(&self.budget, bytes)
    }

    /// Intermediate compressed representations can exceed the native chunk
    /// estimate (nested compression or an outer-compressed shard). Admit their
    /// growing output separately, including a reallocation/copy allowance.
    pub(crate) fn intermediate(&self, additional: usize) -> Result<(), DataServerError> {
        let bytes = (additional as u64)
            .checked_mul(2)
            .ok_or(DataServerError::ResourceExhausted)?;
        self.held.lock().unwrap().reserve(&self.budget, bytes)
    }

    /// Two copies cover the collected body and zarrs' owned encoded buffer.
    /// Transport buffers and compressor-private contexts are separate estimates.
    pub(crate) fn object(&self, key: &str, size: u64) -> Result<(), DataServerError> {
        let mut held = self.held.lock().unwrap();
        let previous = held.objects.get(key).copied().unwrap_or(0);
        if size > previous {
            let bytes = size
                .checked_sub(previous)
                .and_then(|n| n.checked_mul(2))
                .ok_or(DataServerError::ResourceExhausted)?;
            held.reserve(&self.budget, bytes)?;
            held.objects.insert(key.to_owned(), size);
        }
        Ok(())
    }

    #[cfg(feature = "icechunk")]
    pub(crate) fn ranges(&self, size: u64) -> Result<(), DataServerError> {
        let bytes = size
            .checked_mul(2)
            .ok_or(DataServerError::ResourceExhausted)?;
        self.held.lock().unwrap().reserve(&self.budget, bytes)
    }
}

pub(crate) fn current() -> Option<Arc<Context>> {
    CURRENT.with_borrow(Clone::clone)
}

pub(crate) fn enter(budget: Option<Arc<Budget>>) -> Guard {
    enter_prepaid(budget, None, 0)
}

pub(crate) fn enter_prepaid(
    budget: Option<Arc<Budget>>,
    permit: Option<Arc<Permit>>,
    credit: u64,
) -> Guard {
    debug_assert!(credit == 0 || permit.is_some());
    let context = budget.map(|budget| {
        Arc::new(Context {
            budget,
            held: Mutex::new(Held {
                credit,
                ..Default::default()
            }),
            _prepaid: permit,
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
    fn prepaid_credit_covers_actual_allocations_and_retains_its_owner() {
        let budget = Arc::new(Budget::new(180));
        let permit = budget.reserve_bytes(120).unwrap();
        let scope = enter_prepaid(Some(budget.clone()), Some(permit.clone()), 80);
        drop(permit);
        let context = current().unwrap();
        {
            let _deadline = ds_core::deadline::enter(Some(std::time::Instant::now()));
            assert!(matches!(
                context.codec_scratch(1),
                Err(DataServerError::DeadlineExceeded)
            ));
            assert_eq!(budget.metrics(), (120, 180, 0));
        }
        context.object("a", 20).unwrap(); // 40 prepaid
        context.intermediate(15).unwrap(); // 30 prepaid
        context.codec_scratch(30).unwrap(); // 10 prepaid + 20 extra
        assert_eq!(budget.metrics(), (140, 180, 0));
        context.object("a", 25).unwrap(); // same object grows by two copies of 5
        assert_eq!(budget.metrics().0, 150);
        assert!(matches!(
            context.codec_scratch(31),
            Err(DataServerError::ResourceExhausted)
        ));
        assert_eq!(budget.metrics(), (150, 180, 1));
        drop(scope);
        assert_eq!(
            budget.metrics().0,
            150,
            "storage futures keep both reservations live"
        );
        drop(context);
        assert_eq!(budget.metrics().0, 0);
    }

    #[test]
    fn failed_growth_does_not_consume_prepaid_credit() {
        let budget = Arc::new(Budget::new(100));
        let permit = budget.reserve_bytes(100).unwrap();
        let _scope = enter_prepaid(Some(budget.clone()), Some(permit), 20);
        let context = current().unwrap();
        assert!(matches!(
            context.object("a", 11),
            Err(DataServerError::ResourceExhausted)
        ));
        context.object("a", 10).unwrap();
        assert_eq!(budget.metrics(), (100, 100, 1));
        // No prepaid bytes remain; the successful same-size lookup reuses
        // its allowance, while another allocation must reject.
        context.object("a", 10).unwrap();
        assert!(context.codec_scratch(1).is_err());
        assert_eq!(budget.metrics(), (100, 100, 2));
    }

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
