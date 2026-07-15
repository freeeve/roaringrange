//! A cooperative in-flight window (semaphore) for ranged reads on a single-threaded
//! executor — the browser reader's bound on concurrent network fetches. Plain `Cell`s
//! suffice (no atomics: the wasm runtime is single-threaded and native tests drive it from
//! one thread). Kept target-independent so the queueing logic is unit-testable off the
//! browser.
//!
//! **Cancel-safety is load-bearing.** A parked waiter can be dropped mid-wait (its read
//! future abandoned), and a wake delivered to a dead waker is silently lost — with
//! wake-one semantics that permanently starves every live waiter behind it (a full-page
//! hang in the browser). So releases wake **every** parked waiter (each re-evaluates and
//! re-parks if still over the limit — a few no-op polls, never a lost slot), and a dropped
//! [`Acquire`] removes its own queue entry so the queue cannot grow unbounded.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// The window state: `limit == 0` means unbounded (permits are still counted, so lowering
/// the limit later takes effect for new acquisitions immediately).
pub(crate) struct FetchGate {
    limit: Cell<usize>,
    active: Cell<usize>,
    /// Parked waiters as `(id, waker)`; the id ties each entry to its [`Acquire`] so a
    /// re-poll updates in place and a drop removes exactly its own entry.
    waiters: RefCell<VecDeque<(u64, Waker)>>,
    /// Monotonic id source for waiter entries.
    next_id: Cell<u64>,
}

impl FetchGate {
    /// A gate admitting `limit` concurrent permits (`0` = unbounded).
    pub(crate) fn new(limit: usize) -> Rc<Self> {
        Rc::new(FetchGate {
            limit: Cell::new(limit),
            active: Cell::new(0),
            waiters: RefCell::new(VecDeque::new()),
            next_id: Cell::new(0),
        })
    }

    /// Resizes the window and wakes every parked waiter so it re-evaluates against the new
    /// limit (a raised limit admits more; a lowered one re-parks the excess).
    pub(crate) fn set_limit(&self, limit: usize) {
        self.limit.set(limit);
        self.wake_all();
    }

    /// `(limit, active, parked)` — a live view for diagnostics (the JS
    /// `fetchWindowStats`).
    pub(crate) fn stats(&self) -> (usize, usize, usize) {
        (
            self.limit.get(),
            self.active.get(),
            self.waiters.borrow().len(),
        )
    }

    /// Waits for a permit; the returned [`FetchPermit`] frees its slot on drop.
    pub(crate) fn acquire(self: &Rc<Self>) -> Acquire {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        Acquire {
            gate: Rc::clone(self),
            id,
        }
    }

    /// Frees one slot and wakes **every** parked waiter. Wake-one would be cheaper, but a
    /// single dead waker (its `Acquire` dropped after parking) would then swallow the wake
    /// and starve all live waiters behind it; waking all makes a lost wakeup impossible at
    /// the cost of a few no-op re-polls.
    fn release(&self) {
        self.active.set(self.active.get().saturating_sub(1));
        self.wake_all();
    }

    fn wake_all(&self) {
        for (_, w) in self.waiters.borrow_mut().drain(..) {
            w.wake();
        }
    }
}

/// An acquired slot in the window; dropping it frees the slot and wakes the waiters.
pub(crate) struct FetchPermit {
    gate: Rc<FetchGate>,
}

impl Drop for FetchPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// The pending acquisition future returned by [`FetchGate::acquire`].
pub(crate) struct Acquire {
    gate: Rc<FetchGate>,
    id: u64,
}

impl Future for Acquire {
    type Output = FetchPermit;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<FetchPermit> {
        let g = &self.gate;
        if g.limit.get() == 0 || g.active.get() < g.limit.get() {
            // Leave no stale queue entry behind when an earlier poll parked us.
            g.waiters.borrow_mut().retain(|(id, _)| *id != self.id);
            g.active.set(g.active.get() + 1);
            Poll::Ready(FetchPermit {
                gate: Rc::clone(&self.gate),
            })
        } else {
            let mut waiters = g.waiters.borrow_mut();
            match waiters.iter_mut().find(|(id, _)| *id == self.id) {
                Some((_, w)) => *w = cx.waker().clone(),
                None => waiters.push_back((self.id, cx.waker().clone())),
            }
            Poll::Pending
        }
    }
}

impl Drop for Acquire {
    /// A dropped waiter must not linger in the queue: wake-all makes a dead waker harmless
    /// for correctness, but removing it keeps the queue from accumulating abandoned reads.
    fn drop(&mut self) {
        self.gate
            .waiters
            .borrow_mut()
            .retain(|(id, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;

    /// Completes on its second poll — one cooperative yield, so permit holders interleave.
    fn yield_once() -> impl Future<Output = ()> {
        struct Y(bool);
        impl Future for Y {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.0 {
                    Poll::Ready(())
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
        Y(false)
    }

    /// Runs `n` tasks that acquire, hold across one yield, and release — returning the peak
    /// number of permits held at once.
    fn peak_concurrency(limit: usize, n: usize) -> usize {
        let gate = FetchGate::new(limit);
        let concurrent = Rc::new(Cell::new(0usize));
        let peak = Rc::new(Cell::new(0usize));
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        for _ in 0..n {
            let gate = gate.clone();
            let concurrent = concurrent.clone();
            let peak = peak.clone();
            spawner
                .spawn_local(async move {
                    let _permit = gate.acquire().await;
                    concurrent.set(concurrent.get() + 1);
                    peak.set(peak.get().max(concurrent.get()));
                    yield_once().await;
                    concurrent.set(concurrent.get() - 1);
                })
                .unwrap();
        }
        pool.run();
        peak.get()
    }

    #[test]
    fn window_caps_concurrency() {
        assert_eq!(peak_concurrency(2, 8), 2);
        assert_eq!(peak_concurrency(3, 3), 3);
        assert_eq!(peak_concurrency(1, 5), 1);
    }

    #[test]
    fn zero_limit_is_unbounded() {
        assert_eq!(peak_concurrency(0, 8), 8);
    }

    /// The v0.36.0 production deadlock (task 088): a waiter dropped while parked must not
    /// swallow the next wake — live waiters behind it still acquire when the slot frees.
    #[test]
    fn dropped_parked_waiter_cannot_starve_live_waiters() {
        use futures::task::noop_waker;
        use futures::FutureExt;

        let gate = FetchGate::new(1);
        // A holds the only permit.
        let a = gate.acquire().now_or_never().expect("first slot acquires");
        // B parks behind A, then its read future is abandoned mid-wait.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut b = Box::pin(gate.acquire());
        assert!(b.as_mut().poll(&mut cx).is_pending(), "B parks behind A");
        // C parks behind A too and stays live.
        let mut pool = LocalPool::new();
        let done = Rc::new(Cell::new(false));
        let (cg, cd) = (gate.clone(), done.clone());
        pool.spawner()
            .spawn_local(async move {
                let _p = cg.acquire().await;
                cd.set(true);
            })
            .unwrap();
        pool.run_until_stalled();
        assert!(!done.get(), "C is parked while A holds the permit");

        drop(b); // the abandoned read
        drop(a); // the slot frees — C must be woken despite B's corpse
        pool.run();
        assert!(done.get(), "a dropped waiter must not starve the window");
    }

    /// Re-polling a parked waiter updates its entry in place, and dropping it removes the
    /// entry — the queue reflects live waiters only.
    #[test]
    fn parked_queue_holds_one_entry_per_live_waiter() {
        use futures::task::noop_waker;
        use futures::FutureExt;

        let gate = FetchGate::new(1);
        let _a = gate.acquire().now_or_never().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut b = Box::pin(gate.acquire());
        for _ in 0..3 {
            assert!(b.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(
            gate.stats(),
            (1, 1, 1),
            "re-polls must not duplicate the entry"
        );
        drop(b);
        assert_eq!(gate.stats(), (1, 1, 0), "a dropped waiter leaves no entry");
    }

    #[test]
    fn raising_the_limit_wakes_parked_waiters() {
        let gate = FetchGate::new(1);
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let done = Rc::new(Cell::new(0usize));
        // Task A holds the only permit across several yields, so B parks behind it.
        let hold = gate.clone();
        let a_done = done.clone();
        spawner
            .spawn_local(async move {
                let permit = hold.acquire().await;
                for _ in 0..3 {
                    yield_once().await;
                }
                a_done.set(a_done.get() + 1);
                drop(permit);
            })
            .unwrap();
        let waitg = gate.clone();
        let b_done = done.clone();
        spawner
            .spawn_local(async move {
                let _p = waitg.acquire().await;
                b_done.set(b_done.get() + 1);
            })
            .unwrap();
        pool.run_until_stalled();
        gate.set_limit(2);
        pool.run();
        assert_eq!(done.get(), 2, "widening the window must admit B");
    }
}
