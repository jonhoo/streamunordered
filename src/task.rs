use alloc::sync::{Arc, Weak};
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering::{self, SeqCst};
use core::sync::atomic::{AtomicBool, AtomicPtr};

use super::abort::abort;
use super::ReadyToRunQueue;
use futures_util::task::{waker_ref, ArcWake, WakerRef};

pub(super) struct Task<S> {
    // The stream
    pub(super) stream: UnsafeCell<Option<S>>,

    // Indicator that the stream has already completed.
    pub(super) is_done: UnsafeCell<bool>,

    // Next pointer for linked list tracking all active tasks (use
    // `spin_next_all` to read when access is shared across threads)
    pub(super) next_all: AtomicPtr<Task<S>>,

    // Previous task in linked list tracking all active tasks
    pub(super) prev_all: UnsafeCell<*const Task<S>>,

    // Length of the linked list tracking all active tasks when this node was
    // inserted (use `spin_next_all` to synchronize before reading when access
    // is shared across threads)
    pub(super) len_all: UnsafeCell<usize>,

    // Next pointer in ready to run queue
    pub(super) next_ready_to_run: AtomicPtr<Task<S>>,

    // Queue that we'll be enqueued to when woken
    pub(super) ready_to_run_queue: Weak<ReadyToRunQueue<S>>,

    // Whether or not this task is currently in the ready to run queue
    pub(super) queued: AtomicBool,

    // A unique identifier for this stream
    pub(super) id: usize,
}

// `Task` can be sent across threads safely because it ensures that
// the underlying `S` type isn't touched from any of its methods.
//
// The parent (`super`) module is trusted not to access `stream`
// across different threads.
unsafe impl<S> Send for Task<S> {}
unsafe impl<S> Sync for Task<S> {}

impl<S> ArcWake for Task<S> {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let inner = match arc_self.ready_to_run_queue.upgrade() {
            Some(inner) => inner,
            None => return,
        };

        // It's our job to enqueue this task it into the ready to run queue. To
        // do this we set the `queued` flag, and if successful we then do the
        // actual queueing operation, ensuring that we're only queued once.
        //
        // Once the task is inserted call `wake` to notify the parent task,
        // as it'll want to come along and run our task later.
        //
        // Note that we don't change the reference count of the task here,
        // we merely enqueue the raw pointer. The `StreamUnordered`
        // implementation guarantees that if we set the `queued` flag that
        // there's a reference count held by the main `StreamUnordered` queue
        // still.
        let prev = arc_self.queued.swap(true, SeqCst);
        if !prev {
            inner.enqueue(&**arc_self);
            inner.waker.wake();
        }
    }
}

impl<S: 'static> Task<S> {
    /// Returns a waker reference for this task without cloning the Arc.
    pub(super) fn waker_ref(this: &Arc<Task<S>>) -> WakerRef<'_> {
        waker_ref(this)
    }

    /// Spins until `next_all` is no longer set to `pending_next_all`.
    ///
    /// The temporary `pending_next_all` value is typically overwritten fairly
    /// quickly after a node is inserted into the list of all streams, so this
    /// should rarely spin much.
    ///
    /// When it returns, the correct `next_all` value is returned.
    ///
    /// `Relaxed` or `Acquire` ordering can be used. `Acquire` ordering must be
    /// used before `len_all` can be safely read.
    #[inline]
    pub(super) fn spin_next_all(
        &self,
        pending_next_all: *mut Self,
        ordering: Ordering,
    ) -> *const Self {
        loop {
            let next = self.next_all.load(ordering);
            if next != pending_next_all {
                return next;
            }
        }
    }
}

impl<S> Drop for Task<S> {
    fn drop(&mut self) {
        // Since `Task<S>` is sent across all threads for any lifetime,
        // regardless of `S`, we, to guarantee memory safety, can't actually
        // touch `S` at any time except when we have a reference to the
        // `StreamUnordered` itself .
        //
        // Consequently it *should* be the case that we always drop streams from
        // the `StreamUnordered` instance. This is a bomb, just in case there's
        // a bug in that logic.
        unsafe {
            if (*self.stream.get()).is_some() {
                abort("stream still here when dropping");
            }
        }
    }
}
