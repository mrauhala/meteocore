//! The elastic ledger (#1314): every loan of memory elastic memory made,
//! between two caches or between a cache and the pool's free headroom,
//! pure. Each cache's offset, what the re-plan tick and a reload's plan add
//! to its grant, is what it borrowed less what it lent, so the two sides of
//! every move stay together: dropping a cache drops its loans, and its
//! counterparts shed exactly those bytes. What caches borrowed always
//! equals what caches and the pool lent.

use ds_cache::plan::SizeSource;
use ds_cache::MIB;

use super::decide::Key;

/// A cache in a loan, with what its size came from when it moved: a reload
/// that pins it, re-bounds it or gives it another configured size ends its
/// loans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Party {
    pub key: Key,
    pub source: SizeSource,
    pub configured: u64,
}

/// `bytes` lent by `lender` to `borrower`, each a cache or, `None`, the
/// pool's free headroom; never the pool on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Loan {
    pub borrower: Option<Party>,
    pub lender: Option<Party>,
    pub bytes: u64,
}

/// A cache's grant, floor and ceiling, which its offset stays within.
pub(crate) type Bounds = (u64, u64, u64);

/// The key of a side of a loan; `None` for the pool.
fn key_of(side: &Option<Party>) -> Option<&Key> {
    side.as_ref().map(|party| &party.key)
}

/// Every loan, at most one a borrower and a lender.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ledger {
    loans: Vec<Loan>,
}

impl Ledger {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.loans.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.loans.clear();
    }

    pub(crate) fn loans(&self) -> &[Loan] {
        &self.loans
    }

    /// What `key` borrowed less what it lent.
    pub(crate) fn offset(&self, key: &Key) -> i64 {
        self.borrowed(key) as i64 - self.lent(key) as i64
    }

    /// What `key` borrowed.
    pub(crate) fn borrowed(&self, key: &Key) -> u64 {
        self.loans
            .iter()
            .filter(|loan| key_of(&loan.borrower) == Some(key))
            .map(|loan| loan.bytes)
            .sum()
    }

    /// What `key` lent.
    pub(crate) fn lent(&self, key: &Key) -> u64 {
        self.loans
            .iter()
            .filter(|loan| key_of(&loan.lender) == Some(key))
            .map(|loan| loan.bytes)
            .sum()
    }

    /// What the pool lent less what it borrowed: what every cache's offset
    /// sums to.
    pub(crate) fn pool_net(&self) -> i64 {
        self.loans
            .iter()
            .map(|loan| match (&loan.borrower, &loan.lender) {
                (Some(_), None) => loan.bytes as i64,
                (None, Some(_)) => -(loan.bytes as i64),
                _ => 0,
            })
            .sum()
    }

    /// Its invariant: no loan empty, between the pool and itself or a cache
    /// and itself; every cache's offset summing to [`Ledger::pool_net`].
    pub(crate) fn balanced(&self) -> Result<(), String> {
        for loan in &self.loans {
            if loan.bytes == 0 {
                return Err("an empty loan".to_string());
            }
            if key_of(&loan.borrower) == key_of(&loan.lender) {
                return Err("a loan to its own lender".to_string());
            }
        }
        let offsets: i64 = self.keys().iter().map(|key| self.offset(key)).sum();
        if offsets != self.pool_net() {
            return Err(format!(
                "its offsets sum to {offsets} bytes, the pool's loans to {}",
                self.pool_net()
            ));
        }
        Ok(())
    }

    /// `key`'s largest loan to another cache: the borrower and the bytes.
    pub(crate) fn largest_loan_to_a_cache(&self, key: &Key) -> Option<(Key, u64)> {
        self.loans
            .iter()
            .filter(|loan| key_of(&loan.lender) == Some(key))
            .filter_map(|loan| Some((key_of(&loan.borrower)?.clone(), loan.bytes)))
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
    }

    /// Every cache with a loan, once each.
    pub(crate) fn keys(&self) -> Vec<Key> {
        let mut keys: Vec<Key> = self
            .loans
            .iter()
            .flat_map(|loan| [key_of(&loan.borrower), key_of(&loan.lender)])
            .flatten()
            .cloned()
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Book a move of `bytes` from `lender` to `borrower`: it repays what
    /// the borrower lent the lender first, and lends the rest.
    pub(crate) fn book(&mut self, borrower: Option<Party>, lender: Option<Party>, bytes: u64) {
        let mut left = bytes;
        for loan in self.loans.iter_mut() {
            if left == 0 {
                break;
            }
            if key_of(&loan.borrower) == key_of(&lender)
                && key_of(&loan.lender) == key_of(&borrower)
            {
                let repaid = left.min(loan.bytes);
                loan.bytes -= repaid;
                left -= repaid;
            }
        }
        if left > 0 && (borrower.is_some() || lender.is_some()) {
            let same = self.loans.iter_mut().find(|loan| {
                key_of(&loan.borrower) == key_of(&borrower)
                    && key_of(&loan.lender) == key_of(&lender)
            });
            match same {
                Some(loan) => loan.bytes += left,
                None => self.loans.push(Loan {
                    borrower,
                    lender,
                    bytes: left,
                }),
            }
        }
        self.loans.retain(|loan| loan.bytes > 0);
    }

    /// End every loan `key` is party to: its counterparts shed them. What
    /// the pressure valve does to a cache it acts on, and a reload to a
    /// cache it rebuilds or reconfigures.
    pub(crate) fn drop_cache(&mut self, key: &Key) {
        self.loans.retain(|loan| {
            key_of(&loan.borrower) != Some(key) && key_of(&loan.lender) != Some(key)
        });
    }

    /// End every loan of a cache the pressure valve lowered: what it
    /// borrowed goes back to the pool, its lenders staying where they are
    /// ([`Ledger::release`]); what it lent ends, its borrowers shedding it.
    /// Its restore then climbs to its plan, and it borrows again only by
    /// elastic memory's steps.
    pub(crate) fn end(&mut self, key: &Key) {
        self.release(key);
        self.drop_cache(key);
    }

    /// Give back what `key` borrowed, to the pool rather than its lenders:
    /// its lenders' loans to it become loans to the pool, so they stay where
    /// they are and the memory leaves the caches; what it borrowed from the
    /// pool ends. Returns the bytes it gave back. What the pressure valve
    /// takes back first.
    pub(crate) fn release(&mut self, key: &Key) -> u64 {
        let mut released = 0;
        let mut to_pool = Vec::new();
        self.loans.retain(|loan| {
            if key_of(&loan.borrower) != Some(key) {
                return true;
            }
            released += loan.bytes;
            if let Some(lender) = &loan.lender {
                to_pool.push((lender.clone(), loan.bytes));
            }
            false
        });
        for (lender, bytes) in to_pool {
            self.book(None, Some(lender), bytes);
        }
        released
    }

    /// Give back `bytes` of what `key` borrowed, as [`Ledger::release`]
    /// does all of it: what it borrowed from the pool first, then from its
    /// lenders, the ones `value` ranks lowest first, each lender's share
    /// becoming its loan to the pool. Returns the bytes given back: at most
    /// what it borrowed. The valve's take-back, for the excess alone.
    pub(crate) fn release_part(
        &mut self,
        key: &Key,
        bytes: u64,
        value: &dyn Fn(&Key) -> f64,
    ) -> u64 {
        let mut order: Vec<usize> = (0..self.loans.len())
            .filter(|&i| key_of(&self.loans[i].borrower) == Some(key))
            .collect();
        let rank = |loan: &Loan| key_of(&loan.lender).map_or(f64::NEG_INFINITY, value);
        order.sort_by(|&a, &b| rank(&self.loans[a]).total_cmp(&rank(&self.loans[b])));
        let mut left = bytes;
        let mut to_pool = Vec::new();
        for i in order {
            if left == 0 {
                break;
            }
            let shed = left.min(self.loans[i].bytes);
            self.loans[i].bytes -= shed;
            left -= shed;
            if let Some(lender) = &self.loans[i].lender {
                to_pool.push((lender.clone(), shed));
            }
        }
        self.loans.retain(|loan| loan.bytes > 0);
        for (lender, shed) in to_pool {
            self.book(None, Some(lender), shed);
        }
        bytes - left
    }

    /// What `key` lent the pool: what a return gives back ([`MoveKind::Return`]).
    ///
    /// [`MoveKind::Return`]: super::decide::MoveKind::Return
    pub(crate) fn lent_to_pool(&self, key: &Key) -> u64 {
        self.loans
            .iter()
            .filter(|loan| loan.borrower.is_none() && key_of(&loan.lender) == Some(key))
            .map(|loan| loan.bytes)
            .sum()
    }

    /// End the loans of every cache `current` no longer gives the size
    /// source and configured size it moved under (a reload pinned,
    /// re-bounded or resized it), or no longer lists.
    pub(crate) fn reconcile(&mut self, current: &dyn Fn(&Key) -> Option<(SizeSource, u64)>) {
        let stale: Vec<Key> = self
            .loans
            .iter()
            .flat_map(|loan| [&loan.borrower, &loan.lender])
            .flatten()
            .filter(|party| {
                current(&party.key).is_none_or(|(source, configured)| {
                    source != party.source || configured != party.configured
                })
            })
            .map(|party| party.key.clone())
            .collect();
        for key in stale {
            self.drop_cache(&key);
        }
    }

    /// End the loans of the caches of `collections`, which a reload rebuilt
    /// or removed.
    pub(crate) fn forget(&mut self, collections: &[&str]) {
        let gone: Vec<Key> = self
            .keys()
            .into_iter()
            .filter(|(_, _, scope)| scope.collection().is_some_and(|c| collections.contains(&c)))
            .collect();
        for key in gone {
            self.drop_cache(&key);
        }
    }

    /// Shrink `key`'s loans by `bytes`, as borrower (`borrowing`) or as
    /// lender, the counterparts `value` ranks lowest first, the pool before
    /// any cache. Returns the bytes shed.
    pub(crate) fn shrink(
        &mut self,
        key: &Key,
        borrowing: bool,
        bytes: u64,
        value: &dyn Fn(&Key) -> f64,
    ) -> u64 {
        let mut order: Vec<usize> = (0..self.loans.len())
            .filter(|&i| {
                let loan = &self.loans[i];
                let side = if borrowing {
                    &loan.borrower
                } else {
                    &loan.lender
                };
                key_of(side) == Some(key)
            })
            .collect();
        let counterpart = |loan: &Loan| {
            let other = if borrowing {
                &loan.lender
            } else {
                &loan.borrower
            };
            key_of(other).map_or(f64::NEG_INFINITY, value)
        };
        order.sort_by(|&a, &b| counterpart(&self.loans[a]).total_cmp(&counterpart(&self.loans[b])));
        let mut left = bytes;
        for i in order {
            let shed = left.min(self.loans[i].bytes);
            self.loans[i].bytes -= shed;
            left -= shed;
            if left == 0 {
                break;
            }
        }
        self.loans.retain(|loan| loan.bytes > 0);
        bytes - left
    }

    /// Hold every cache's offset within its bounds (`bounds`: its grant,
    /// floor and ceiling, `None` for a cache the plan no longer sizes, whose
    /// loans end): a cache below its floor sheds what it lent, one above its
    /// ceiling what it borrowed, the lowest-valued counterparts first, by
    /// whole MiB.
    pub(crate) fn bound(
        &mut self,
        bounds: &dyn Fn(&Key) -> Option<Bounds>,
        value: &dyn Fn(&Key) -> f64,
    ) {
        for key in self.keys() {
            let Some((grant, floor, ceiling)) = bounds(&key) else {
                self.drop_cache(&key);
                continue;
            };
            let target = grant as i64 + self.offset(&key);
            if target < floor as i64 {
                let over = (floor as i64 - target) as u64;
                self.shrink(&key, false, over.div_ceil(MIB) * MIB, value);
            } else if target > ceiling as i64 {
                let over = (target - ceiling as i64) as u64;
                self.shrink(&key, true, over.div_ceil(MIB) * MIB, value);
            }
        }
    }
}
