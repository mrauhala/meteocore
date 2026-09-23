//! Encoded-buffer admission for one synchronous native retrieval.
//!
//! zarrs converts storage Bytes into owned codec Vecs, so a guard attached only
//! to Bytes would release too early. Keep encoded/intermediate reservations
//! until retrieval returns. Blosc scratch has separate guards that end once
//! the native call has freed its temporary buffers.
//! Repeated plain whole-object lookups share an allowance within this scope;
//! range operations accumulate conservatively. Each Icechunk chunk job gets a
//! separate scope, bounding retention to its decode rather than the whole map.
//! Plain/outer-transform reads likewise scope each stored chunk in retrieval.rs;
//! all nested shard/index copies stay admitted until that chunk finishes.
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
        let (_, permit) = self.allocate(budget, bytes)?;
        if let Some(permit) = permit {
            self.permits.push(permit);
        }
        Ok(())
    }

    fn allocate(
        &mut self,
        budget: &Arc<Budget>,
        bytes: u64,
    ) -> Result<(u64, Option<Arc<Permit>>), DataServerError> {
        ds_core::deadline::check()?;
        let prepaid = self.credit.min(bytes);
        let permit = (bytes > prepaid)
            .then(|| budget.reserve_bytes(bytes - prepaid))
            .transpose()?;
        self.credit -= prepaid;
        Ok((prepaid, permit))
    }
}

/// Only for native scratch proven to be freed when the guarded call returns.
/// Encoded copies and intermediate outputs still belong to the retrieval scope.
#[must_use = "keep scratch admission alive until the native call returns"]
pub(crate) struct Scratch {
    context: Arc<Context>,
    _permit: Option<Arc<Permit>>,
    credit: u64,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(self._permit.take());
        let mut held = self.context.held.lock().unwrap();
        held.credit = held
            .credit
            .checked_add(self.credit)
            .expect("returning borrowed prepaid credit cannot overflow");
    }
}

impl Context {
    /// Blosc's serial/getitem scratch scales with the validated block size.
    /// Release additional capacity and return borrowed credit after the native
    /// call. Overlapping calls must hold separate guards, even in one context.
    pub(crate) fn codec_scratch(self: &Arc<Self>, bytes: u64) -> Result<Scratch, DataServerError> {
        let (credit, permit) = self.held.lock().unwrap().allocate(&self.budget, bytes)?;
        Ok(Scratch {
            context: self.clone(),
            _permit: permit,
            credit,
        })
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
    fn overlapping_scratch_guards_cannot_share_credit_and_keep_the_owner_alive() {
        let budget = Arc::new(Budget::new(132));
        let permit = budget.reserve_bytes(100).unwrap();
        let scope = enter_prepaid(Some(budget.clone()), Some(permit), 64);
        let context = current().unwrap();
        let first = context.codec_scratch(48).unwrap();
        let second = context.codec_scratch(48).unwrap(); // 16 prepaid + 32 extra
        assert_eq!(budget.metrics(), (132, 132, 0));
        assert!(context.codec_scratch(1).is_err());
        drop(second);
        assert_eq!(budget.metrics(), (100, 132, 1));
        let replacement = context.codec_scratch(48).unwrap();
        assert_eq!(budget.metrics().0, 132);
        drop(scope);
        drop(context);
        drop(first);
        assert_eq!(
            budget.metrics().0,
            132,
            "the last call still owns its allowance"
        );
        drop(replacement);
        assert_eq!(budget.metrics().0, 0);
    }

    #[test]
    fn scratch_refunds_are_isolated_from_encoded_and_intermediate_allowances() {
        let budget = Arc::new(Budget::new(160));
        let permit = budget.reserve_bytes(100).unwrap();
        let scope = enter_prepaid(Some(budget.clone()), Some(permit), 60);
        let context = current().unwrap();
        context.object("payload", 20).unwrap(); // 40 credit retained
        let scratch = context.codec_scratch(40).unwrap(); // 20 credit + 20 extra
        context.intermediate(10).unwrap(); // 20 extra retained
        assert_eq!(budget.metrics().0, 140);
        drop(scratch);
        assert_eq!(budget.metrics().0, 120);
        // The refunded 20 can be reused for an encoded copy, but never refunds
        // the 40 encoded bytes or 20 intermediate bytes still live in zarrs.
        context.object("another", 10).unwrap();
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scratch = context.codec_scratch(40).unwrap();
            assert_eq!(budget.metrics().0, 160);
            panic!("decoder unwind");
        }));
        assert!(failed.is_err());
        assert_eq!(budget.metrics().0, 120);
        assert!(context.codec_scratch(41).is_err());
        drop(context);
        drop(scope);
        assert_eq!(budget.metrics(), (0, 160, 1));
    }

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
        let scratch = context.codec_scratch(30).unwrap(); // 10 prepaid + 20 extra
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
        assert_eq!(budget.metrics().0, 150, "scratch retains the prepaid owner");
        drop(scratch);
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
