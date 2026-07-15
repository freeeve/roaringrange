//! A cooperative in-flight window (semaphore) for ranged reads on a single-threaded
//! executor — the browser reader's bound on concurrent network fetches. Plain `Cell`s
//! suffice (no atomics: the wasm runtime is single-threaded and native tests drive it from
//! one thread); waiters park their `Waker` and are woken FIFO as permits free up. Kept
//! target-independent so the queueing logic is unit-testable off the browser.

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
    waiters: RefCell<VecDeque<Waker>>,
}

impl FetchGate {
    /// A gate admitting `limit` concurrent permits (`0` = unbounded).
    pub(crate) fn new(limit: usize) -> Rc<Self> {
        Rc::new(FetchGate {
            limit: Cell::new(limit),
            active: Cell::new(0),
            waiters: RefCell::new(VecDeque::new()),
        })
    }

    /// Resizes the window and wakes every parked waiter so it re-evaluates against the new
    /// limit (a raised limit admits more; a lowered one re-parks the excess).
    pub(crate) fn set_limit(&self, limit: usize) {
        self.limit.set(limit);
        for w in self.waiters.borrow_mut().drain(..) {
            w.wake();
        }
    }

    /// Waits for a permit; the returned [`FetchPermit`] frees its slot on drop.
    pub(crate) fn acquire(self: &Rc<Self>) -> Acquire {
        Acquire {
            gate: Rc::clone(self),
        }
    }

    /// Frees one slot and wakes the next waiter.
    fn release(&self) {
        self.active.set(self.active.get().saturating_sub(1));
        if let Some(w) = self.waiters.borrow_mut().pop_front() {
            w.wake();
        }
    }
}

/// An acquired slot in the window; dropping it frees the slot and wakes the next waiter.
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
}

impl Future for Acquire {
    type Output = FetchPermit;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<FetchPermit> {
        let g = &self.gate;
        if g.limit.get() == 0 || g.active.get() < g.limit.get() {
            g.active.set(g.active.get() + 1);
            Poll::Ready(FetchPermit {
                gate: Rc::clone(&self.gate),
            })
        } else {
            g.waiters.borrow_mut().push_back(cx.waker().clone());
            Poll::Pending
        }
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
