//! A stream that efficiently multiplexes multiple streams.
//!
//! This "combinator" provides the ability to maintain and drive a set of streams to completion,
//! while also providing access to each stream as it yields new elements.
//!
//! Streams are inserted into this set and their realized values are yielded as they are produced.
//! This structure is optimized to manage a large number of streams. Streams managed by
//! `StreamUnordered` will only be polled when they generate notifications. This reduces the
//! required amount of work needed to coordinate large numbers of streams.
//!
//! When a `StreamUnordered` is first created, it does not contain any streams. Calling `poll` in
//! this state will result in `Poll::Ready((None)` to be returned. Streams are submitted to the
//! set using `insert`; however, the stream will **not** be polled at this point. `StreamUnordered`
//! will only poll managed streams when `StreamUnordered::poll` is called. As such, it is important
//! to call `poll` after inserting new streams.
//!
//! If `StreamUnordered::poll` returns `Poll::Ready(None)` this means that the set is
//! currently not managing any streams. A stream may be submitted to the set at a later time. At
//! that point, a call to `StreamUnordered::poll` will either return the stream's resolved value
//! **or** `Poll::Pending` if the stream has not yet completed.
//!
//! Whenever a value is yielded, the yielding stream's index is also included. A reference to the
//! stream that originated the value is obtained by using [`StreamUnordered::get`],
//! [`StreamUnordered::get_mut`], or [`StreamUnordered::get_pin_mut`].
//!
//! In normal operation, `poll` will yield a `StreamYield::Item` when it completes successfully.
//! This value indicates that an underlying stream (the one indicated by the included index)
//! produced an item. If an underlying stream yields `Poll::Ready(None)` to indicate termination,
//! a `StreamYield::Finished` is returned instead. Note that as soon as a stream returns
//! `StreamYield::Finished`, its token may be reused for new streams that are added.

#![deny(missing_docs)]
#![warn(rust_2018_idioms, rustdoc::broken_intra_doc_links)]

// This is mainly FuturesUnordered from futures_util, but adapted to operate over Streams rather
// than Futures.

extern crate alloc;

use alloc::sync::{Arc, Weak};
use core::cell::UnsafeCell;
use core::fmt::{self, Debug};
use core::iter::FromIterator;
use core::mem;
use core::ops::{Index, IndexMut};
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use core::sync::atomic::{AtomicBool, AtomicPtr};
use futures_core::stream::{FusedStream, Stream};
use futures_core::task::{Context, Poll};
use futures_util::task::{ArcWake, AtomicWaker};
use std::marker::PhantomData;

mod abort;

mod iter;
pub use self::iter::{IterMut, IterMutWithToken, IterPinMut, IterPinMutWithToken, IterWithToken};

mod task;
use self::task::Task;

mod ready_to_run_queue;
use self::ready_to_run_queue::{Dequeue, ReadyToRunQueue};

/// Constant used for a `StreamUnordered` to determine how many times it is
/// allowed to poll underlying futures without yielding.
///
/// A single call to `poll_next` may potentially do a lot of work before
/// yielding. This happens in particular if the underlying futures are awoken
/// frequently but continue to return `Pending`. This is problematic if other
/// tasks are waiting on the executor, since they do not get to run. This value
/// caps the number of calls to `poll` on underlying streams a single call to
/// `poll_next` is allowed to make.
///
/// The value itself is chosen somewhat arbitrarily. It needs to be high enough
/// that amortize wakeup and scheduling costs, but low enough that we do not
/// starve other tasks for long.
///
/// See also https://github.com/rust-lang/futures-rs/issues/2047.
const YIELD_EVERY: usize = 32;

/// A set of streams which may yield items in any order.
///
/// This structure is optimized to manage a large number of streams.
/// Streams managed by [`StreamUnordered`] will only be polled when they
/// generate wake-up notifications. This reduces the required amount of work
/// needed to poll large numbers of streams.
///
/// [`StreamUnordered`] can be filled by [`collect`](Iterator::collect)ing an
/// iterator of streams into a [`StreamUnordered`], or by
/// [`insert`](StreamUnordered::insert)ing streams onto an existing
/// [`StreamUnordered`]. When new streams are added,
/// [`poll_next`](Stream::poll_next) must be called in order to begin receiving
/// wake-ups for new streams.
///
/// Note that you can create a ready-made [`StreamUnordered`] via the
/// [`collect`](Iterator::collect) method, or you can start with an empty set
/// with the [`StreamUnordered::new`] constructor.
#[must_use = "streams do nothing unless polled"]
pub struct StreamUnordered<S> {
    ready_to_run_queue: Arc<ReadyToRunQueue<S>>,
    head_all: AtomicPtr<Task<S>>,
    is_terminated: AtomicBool,
    by_id: slab::Slab<*const Task<S>>,
}

unsafe impl<S: Send> Send for StreamUnordered<S> {}
unsafe impl<S: Sync> Sync for StreamUnordered<S> {}
impl<S> Unpin for StreamUnordered<S> {}

// StreamUnordered is implemented using two linked lists. One which links all
// streams managed by a `StreamUnordered` and one that tracks streams that have
// been scheduled for polling. The first linked list allows for thread safe
// insertion of nodes at the head as well as forward iteration, but is otherwise
// not thread safe and is only accessed by the thread that owns the
// `StreamUnordered` value for any other operations. The second linked list is
// an implementation of the intrusive MPSC queue algorithm described by
// 1024cores.net.
//
// When a stream is submitted to the set, a task is allocated and inserted in
// both linked lists. The next call to `poll_next` will (eventually) see this
// task and call `poll` on the stream.
//
// Before a managed stream is polled, the current context's waker is replaced
// with one that is aware of the specific stream being run. This ensures that
// wake-up notifications generated by that specific stream are visible to
// `StreamUnordered`. When a wake-up notification is received, the task is
// inserted into the ready to run queue, so that its stream can be polled later.
//
// Each task is wrapped in an `Arc` and thereby atomically reference counted.
// Also, each task contains an `AtomicBool` which acts as a flag that indicates
// whether the task is currently inserted in the atomic queue. When a wake-up
// notifiaction is received, the task will only be inserted into the ready to
// run queue if it isn't inserted already.

/// A handle to an vacant stream slot in a `StreamUnordered`.
///
/// `StreamEntry` allows constructing streams that hold the token that they will be assigned.
#[derive(Debug)]
pub struct StreamEntry<'a, S> {
    token: usize,
    inserted: bool,
    backref: &'a mut StreamUnordered<S>,
}

impl<'a, S: 'a> StreamEntry<'a, S> {
    /// Insert a stream in the slot, and return a mutable reference to the value.
    ///
    /// To get the token associated with the stream, use key prior to calling insert.
    pub fn insert(mut self, stream: S) {
        self.inserted = true;

        // this is safe because we've held &mut StreamUnordered the entire time,
        // so the token still points to a valid task, and no-one else is
        // touching the .stream of it.
        unsafe {
            (*(*self.backref.by_id[self.token]).stream.get()) = Some(stream);
        }
    }

    /// Return the token associated with this slot.
    ///
    /// A stream stored in this slot will be associated with this token.
    pub fn token(&self) -> usize {
        self.token
    }
}

impl<'a, S: 'a> Drop for StreamEntry<'a, S> {
    fn drop(&mut self) {
        if !self.inserted {
            // undo the insertion
            let task_ptr = self.backref.by_id.remove(self.token);

            // we know task_ptr points to a valid task, since the StreamEntry
            // has held the &mut StreamUnordered the entire time.
            let task = unsafe { self.backref.unlink(task_ptr) };
            self.backref.release_task(task);
        }
    }
}

impl<S: Stream> StreamUnordered<S> {
    /// Constructs a new, empty [`StreamUnordered`].
    ///
    /// The returned [`StreamUnordered`] does not contain any streams.
    /// In this state, [`StreamUnordered::poll_next`](Stream::poll_next) will
    /// return [`Poll::Ready(None)`](Poll::Ready).
    pub fn new() -> StreamUnordered<S> {
        let mut slab = slab::Slab::new();
        let slot = slab.vacant_entry();
        let stub = Arc::new(Task {
            stream: UnsafeCell::new(None),
            is_done: UnsafeCell::new(false),
            next_all: AtomicPtr::new(ptr::null_mut()),
            prev_all: UnsafeCell::new(ptr::null()),
            len_all: UnsafeCell::new(0),
            next_ready_to_run: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            ready_to_run_queue: Weak::new(),
            id: slot.key(),
        });
        let stub_ptr = &*stub as *const Task<S>;
        let _ = slab.insert(stub_ptr);

        let ready_to_run_queue = Arc::new(ReadyToRunQueue {
            waker: AtomicWaker::new(),
            head: AtomicPtr::new(stub_ptr as *mut _),
            tail: UnsafeCell::new(stub_ptr),
            stub,
        });

        StreamUnordered {
            head_all: AtomicPtr::new(ptr::null_mut()),
            ready_to_run_queue,
            is_terminated: AtomicBool::new(false),
            by_id: slab,
        }
    }
}

impl<S: Stream> Default for StreamUnordered<S> {
    fn default() -> StreamUnordered<S> {
        StreamUnordered::new()
    }
}

impl<S> StreamUnordered<S> {
    /// Returns the number of streams contained in the set.
    ///
    /// This represents the total number of in-flight streams.
    pub fn len(&self) -> usize {
        let (_, len) = self.atomic_load_head_and_len_all();
        len
    }

    /// Returns `true` if the set contains no streams.
    pub fn is_empty(&self) -> bool {
        // Relaxed ordering can be used here since we don't need to read from
        // the head pointer, only check whether it is null.
        self.head_all.load(Relaxed).is_null()
    }

    /// Returns a handle to a vacant stream entry allowing for further manipulation.
    ///
    /// This function is useful when creating values that must contain their stream token. The
    /// returned `StreamEntry` reserves an entry for the stream and is able to query the associated
    /// token.
    pub fn stream_entry<'a>(&'a mut self) -> StreamEntry<'a, S> {
        let next_all = self.pending_next_all();
        let slot = self.by_id.vacant_entry();
        let token = slot.key();

        let task = Arc::new(Task {
            stream: UnsafeCell::new(None),
            is_done: UnsafeCell::new(false),
            next_all: AtomicPtr::new(next_all),
            prev_all: UnsafeCell::new(ptr::null_mut()),
            len_all: UnsafeCell::new(0),
            next_ready_to_run: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            ready_to_run_queue: Arc::downgrade(&self.ready_to_run_queue),
            id: token,
        });

        let _ = slot.insert(&*task as *const _);

        // Reset the `is_terminated` flag if we've previously marked ourselves
        // as terminated.
        self.is_terminated.store(false, Relaxed);

        // Right now our task has a strong reference count of 1. We transfer
        // ownership of this reference count to our internal linked list
        // and we'll reclaim ownership through the `unlink` method below.
        let ptr = self.link(task);

        // We'll need to get the stream "into the system" to start tracking it,
        // e.g. getting its wake-up notifications going to us tracking which
        // streams are ready. To do that we unconditionally enqueue it for
        // polling here.
        self.ready_to_run_queue.enqueue(ptr);

        StreamEntry {
            token,
            inserted: false,
            backref: self,
        }
    }

    /// Insert a stream into the set.
    ///
    /// A deprecated synonym for [`insert`].
    #[deprecated(since = "0.5.2", note = "Prefer StreamUnordered::insert")]
    pub fn push(&mut self, stream: S) -> usize {
        self.insert(stream)
    }

    /// Insert a stream into the set.
    ///
    /// This method adds the given stream to the set. This method will not call
    /// [`poll_next`](futures_util::stream::Stream::poll_next) on the submitted stream. The caller
    /// must ensure that [`StreamUnordered::poll_next`](Stream::poll_next) is called in order to
    /// receive wake-up notifications for the given stream.
    ///
    /// The returned token is an identifier that uniquely identifies the given stream in the
    /// current set. To get a handle to the inserted stream, pass the token to
    /// [`StreamUnordered::get`], [`StreamUnordered::get_mut`], or [`StreamUnordered::get_pin_mut`]
    /// (or just index `StreamUnordered` directly). The same token will be yielded whenever an
    /// element is pulled from this stream.
    ///
    /// Note that the streams are not ordered, and may not be yielded back in insertion or token
    /// order when you iterate over them.
    pub fn insert(&mut self, stream: S) -> usize {
        let s = self.stream_entry();
        let token = s.token();
        s.insert(stream);
        token
    }

    /// Remove a stream from the set.
    ///
    /// The stream will be dropped and will no longer yield stream events.
    pub fn remove(mut self: Pin<&mut Self>, token: usize) -> bool {
        if token == 0 {
            return false;
        }

        let task = if let Some(task) = self.by_id.get(token) {
            *task
        } else {
            return false;
        };

        // we know that by_id only references valid tasks
        let task = unsafe { self.unlink(task) };
        self.release_task(task);
        true
    }

    /// Remove and return a stream from the set.
    ///
    /// The stream will no longer be polled, and will no longer yield stream events.
    ///
    /// Note that since this method moves `S`, which we may have given out a `Pin` to, it requires
    /// that `S` is `Unpin`.
    pub fn take(mut self: Pin<&mut Self>, token: usize) -> Option<S>
    where
        S: Unpin,
    {
        if token == 0 {
            return None;
        }

        let task = *self.by_id.get(token)?;

        // we know that by_id only references valid tasks
        let task = unsafe { self.unlink(task) };

        // This is safe because we're dropping the stream on the thread that owns
        // `StreamUnordered`, which correctly tracks `S`'s lifetimes and such.
        // The logic is the same as for why release_task is allowed to touch task.stream.
        // Since S: Unpin, it is okay for us to move S.
        let stream = unsafe { &mut *task.stream.get() }.take();

        self.release_task(task);

        stream
    }

    /// Returns `true` if the stream with the given token has yielded `None`.
    pub fn is_finished(&self, token: usize) -> Option<bool> {
        if token == 0 {
            return None;
        }

        // we know that by_id only references valid tasks
        Some(unsafe { *(**self.by_id.get(token)?).is_done.get() })
    }

    /// Returns a reference to the stream with the given token
    pub fn get<'a>(&'a self, token: usize) -> Option<&'a S> {
        // don't allow access to the 0th task, since it's not a stream
        if token == 0 {
            return None;
        }

        // we know that by_id only references valid tasks
        Some(unsafe { (*(**self.by_id.get(token)?).stream.get()).as_ref().unwrap() })
    }

    /// Returns a reference that allows modifying the stream with the given token.
    pub fn get_mut<'a>(&'a mut self, token: usize) -> Option<&'a mut S>
    where
        S: Unpin,
    {
        // don't allow access to the 0th task, since it's not a stream
        if token == 0 {
            return None;
        }

        // this is safe for the same reason that IterMut::next is safe
        Some(unsafe {
            (*(**self.by_id.get_mut(token)?).stream.get())
                .as_mut()
                .unwrap()
        })
    }

    /// Returns a pinned reference that allows modifying the stream with the given token.
    pub fn get_pin_mut<'a>(mut self: Pin<&'a mut Self>, token: usize) -> Option<Pin<&'a mut S>> {
        // don't allow access to the 0th task, since it's not a stream
        if token == 0 {
            return None;
        }

        // this is safe for the same reason that IterPinMut::next is safe
        Some(unsafe {
            Pin::new_unchecked(
                (*(**self.by_id.get_mut(token)?).stream.get())
                    .as_mut()
                    .unwrap(),
            )
        })
    }

    /// Returns an iterator that allows modifying each stream in the set.
    pub fn iter_mut(&mut self) -> IterMut<'_, S>
    where
        S: Unpin,
    {
        IterMut(Pin::new(self).iter_pin_mut_with_token())
    }

    /// Returns an iterator that allows modifying each stream in the set.
    pub fn iter_mut_with_token(&mut self) -> IterMutWithToken<'_, S>
    where
        S: Unpin,
    {
        IterMutWithToken(Pin::new(self).iter_pin_mut_with_token())
    }

    /// Returns an iterator that allows modifying each stream in the set.
    pub fn iter_pin_mut(self: Pin<&mut Self>) -> IterPinMut<'_, S> {
        IterPinMut(self.iter_pin_mut_with_token())
    }

    /// Returns an iterator that allows modifying each stream in the set.
    pub fn iter_pin_mut_with_token(mut self: Pin<&mut Self>) -> IterPinMutWithToken<'_, S> {
        // `head_all` can be accessed directly and we don't need to spin on
        // `Task::next_all` since we have exclusive access to the set.
        let task = *self.head_all.get_mut();
        let len = if task.is_null() {
            0
        } else {
            unsafe { *(*task).len_all.get() }
        };

        IterPinMutWithToken {
            task,
            len,
            _marker: PhantomData,
        }
    }

    /// Returns an immutable iterator that allows getting a reference to each stream in the set.
    pub fn iter_with_token(&self) -> IterWithToken<'_, S> {
        let (task, len) = self.atomic_load_head_and_len_all();
        IterWithToken {
            task,
            len,
            pending_next_all: self.pending_next_all(),
            _marker: PhantomData,
        }
    }

    /// Returns the current head node and number of streams in the list of all
    /// streams within a context where access is shared with other threads
    /// (mostly for use with the `len` and `iter_pin_ref` methods).
    fn atomic_load_head_and_len_all(&self) -> (*const Task<S>, usize) {
        let task = self.head_all.load(Acquire);
        let len = if task.is_null() {
            0
        } else {
            unsafe {
                (*task).spin_next_all(self.pending_next_all(), Acquire);
                *(*task).len_all.get()
            }
        };

        (task, len)
    }

    /// Releases the task. It destorys the stream inside and either drops
    /// the `Arc<Task>` or transfers ownership to the ready to run queue.
    /// The task this method is called on must have been unlinked before.
    fn release_task(&mut self, task: Arc<Task<S>>) {
        self.by_id.remove(task.id);

        // `release_task` must only be called on unlinked tasks
        debug_assert_eq!(task.next_all.load(Relaxed), self.pending_next_all());
        unsafe {
            debug_assert!((*task.prev_all.get()).is_null());
        }

        // The stream is done, try to reset the queued flag. This will prevent
        // `wake` from doing any work in the stream
        let prev = task.queued.swap(true, SeqCst);

        // Drop the stream, even if it hasn't finished yet. This is safe
        // because we're dropping the stream on the thread that owns
        // `StreamUnordered`, which correctly tracks `S`'s lifetimes and
        // such.
        unsafe {
            // Set to `None` rather than `take()`ing to prevent moving the
            // stream.
            *task.stream.get() = None;
        }

        // If the queued flag was previously set, then it means that this task
        // is still in our internal ready to run queue. We then transfer
        // ownership of our reference count to the ready to run queue, and it'll
        // come along and free it later, noticing that the stream is `None`.
        //
        // If, however, the queued flag was *not* set then we're safe to
        // release our reference count on the task. The queued flag was set
        // above so all stream `enqueue` operations will not actually
        // enqueue the task, so our task will never see the ready to run queue
        // again. The task itself will be deallocated once all reference counts
        // have been dropped elsewhere by the various wakers that contain it.
        if prev {
            mem::forget(task);
        }
    }

    /// Insert a new task into the internal linked list.
    fn link(&self, task: Arc<Task<S>>) -> *const Task<S> {
        // `next_all` should already be reset to the pending state before this
        // function is called.
        debug_assert_eq!(task.next_all.load(Relaxed), self.pending_next_all());
        let ptr = Arc::into_raw(task);

        // Atomically swap out the old head node to get the node that should be
        // assigned to `next_all`.
        let next = self.head_all.swap(ptr as *mut _, AcqRel);

        unsafe {
            // Store the new list length in the new node.
            let new_len = if next.is_null() {
                1
            } else {
                // Make sure `next_all` has been written to signal that it is
                // safe to read `len_all`.
                (*next).spin_next_all(self.pending_next_all(), Acquire);
                *(*next).len_all.get() + 1
            };
            *(*ptr).len_all.get() = new_len;

            // Write the old head as the next node pointer, signaling to other
            // threads that `len_all` and `next_all` are ready to read.
            (*ptr).next_all.store(next, Release);

            // `prev_all` updates don't need to be synchronized, as the field is
            // only ever used after exclusive access has been acquired.
            if !next.is_null() {
                *(*next).prev_all.get() = ptr;
            }
        }

        ptr
    }

    /// Remove the task from the linked list tracking all tasks currently
    /// managed by `StreamUnordered`.
    /// This method is unsafe because it has be guaranteed that `task` is a
    /// valid pointer.
    unsafe fn unlink(&mut self, task: *const Task<S>) -> Arc<Task<S>> {
        // Compute the new list length now in case we're removing the head node
        // and won't be able to retrieve the correct length later.
        let head = *self.head_all.get_mut();
        debug_assert!(!head.is_null());
        let new_len = *(*head).len_all.get() - 1;

        let task = Arc::from_raw(task);
        let next = task.next_all.load(Relaxed);
        let prev = *task.prev_all.get();
        task.next_all.store(self.pending_next_all(), Relaxed);
        *task.prev_all.get() = ptr::null_mut();

        if !next.is_null() {
            *(*next).prev_all.get() = prev;
        }

        if !prev.is_null() {
            (*prev).next_all.store(next, Relaxed);
        } else {
            *self.head_all.get_mut() = next;
        }

        // Store the new list length in the head node.
        let head = *self.head_all.get_mut();
        if !head.is_null() {
            *(*head).len_all.get() = new_len;
        }

        task
    }

    /// Returns the reserved value for `Task::next_all` to indicate a pending
    /// assignment from the thread that inserted the task.
    ///
    /// `StreamUnordered::link` needs to update `Task` pointers in an order
    /// that ensures any iterators created on other threads can correctly
    /// traverse the entire `Task` list using the chain of `next_all` pointers.
    /// This could be solved with a compare-exchange loop that stores the
    /// current `head_all` in `next_all` and swaps out `head_all` with the new
    /// `Task` pointer if the head hasn't already changed. Under heavy thread
    /// contention, this compare-exchange loop could become costly.
    ///
    /// An alternative is to initialize `next_all` to a reserved pending state
    /// first, perform an atomic swap on `head_all`, and finally update
    /// `next_all` with the old head node. Iterators will then either see the
    /// pending state value or the correct next node pointer, and can reload
    /// `next_all` as needed until the correct value is loaded. The number of
    /// retries needed (if any) would be small and will always be finite, so
    /// this should generally perform better than the compare-exchange loop.
    ///
    /// A valid `Task` pointer in the `head_all` list is guaranteed to never be
    /// this value, so it is safe to use as a reserved value until the correct
    /// value can be written.
    fn pending_next_all(&self) -> *mut Task<S> {
        // The `ReadyToRunQueue` stub is never inserted into the `head_all`
        // list, and its pointer value will remain valid for the lifetime of
        // this `StreamUnordered`, so we can make use of its value here.
        &*self.ready_to_run_queue.stub as *const _ as *mut _
    }
}

impl<S> Index<usize> for StreamUnordered<S> {
    type Output = S;

    fn index(&self, stream: usize) -> &Self::Output {
        self.get(stream).unwrap()
    }
}

impl<S> IndexMut<usize> for StreamUnordered<S>
where
    S: Unpin,
{
    fn index_mut(&mut self, stream: usize) -> &mut Self::Output {
        self.get_mut(stream).unwrap()
    }
}

/// An event that occurred for a managed stream.
pub enum StreamYield<S>
where
    S: Stream,
{
    /// The underlying stream produced an item.
    Item(S::Item),
    /// The underlying stream has completed.
    Finished(FinishedStream),
}

/// A stream that has yielded all the items it ever will.
///
/// The underlying stream will only be dropped by explicitly removing it from the associated
/// `StreamUnordered`. This method is marked as `#[must_use]` to ensure that you either remove the
/// stream immediately, or you explicitly ask for it to be kept around for later use.
///
/// If the `FinishedStream` is dropped, the exhausted stream will not be dropped until the owning
/// `StreamUnordered` is.
#[must_use]
pub struct FinishedStream {
    token: usize,
}

impl FinishedStream {
    /// Remove the exhausted stream.
    ///
    /// See [`StreamUnordered::remove`].
    pub fn remove<S>(self, so: Pin<&mut StreamUnordered<S>>) {
        so.remove(self.token);
    }

    /// Take the exhausted stream.
    ///
    /// Note that this requires `S: Unpin` since it moves the stream even though it has already
    /// been pinned by `StreamUnordered`.
    ///
    /// See [`StreamUnordered::take`].
    pub fn take<S>(self, so: Pin<&mut StreamUnordered<S>>) -> Option<S>
    where
        S: Unpin,
    {
        so.take(self.token)
    }

    /// Leave the exhausted stream in the `StreamUnordered`.
    ///
    /// This allows you to continue to access the stream through [`StreamUnordered::get_mut`] and
    /// friends should you need to perform further operations on it (e.g., if it is also being used
    /// as a `Sink`). Note that the stream will then not be dropped until you explicitly `remove`
    /// or `take` it from the `StreamUnordered`.
    pub fn keep(self) {}

    /// Return the token associated with the exhausted stream.
    pub fn token(self) -> usize {
        self.token
    }
}

impl<S> Debug for StreamYield<S>
where
    S: Stream,
    S::Item: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamYield::Item(ref i) => f.debug_tuple("StreamYield::Item").field(i).finish(),
            StreamYield::Finished(_) => f.debug_tuple("StreamYield::Finished").finish(),
        }
    }
}

impl<S> PartialEq for StreamYield<S>
where
    S: Stream,
    S::Item: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (&StreamYield::Item(ref s), &StreamYield::Item(ref o)) => s == o,
            _ => false,
        }
    }
}

impl<S: Stream> Stream for StreamUnordered<S> {
    type Item = (StreamYield<S>, usize);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Keep track of how many child futures we have polled,
        // in case we want to forcibly yield.
        let mut polled = 0;

        // Ensure `parent` is correctly set.
        self.ready_to_run_queue.waker.register(cx.waker());

        loop {
            // Safety: &mut self guarantees the mutual exclusion `dequeue`
            // expects
            let task = match unsafe { self.ready_to_run_queue.dequeue() } {
                Dequeue::Empty => {
                    if self.is_empty() {
                        // We can only consider ourselves terminated once we
                        // have yielded a `None`
                        *self.is_terminated.get_mut() = true;
                        return Poll::Ready(None);
                    } else {
                        return Poll::Pending;
                    }
                }
                Dequeue::Inconsistent => {
                    // At this point, it may be worth yielding the thread &
                    // spinning a few times... but for now, just yield using the
                    // task system.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Dequeue::Data(task) => task,
            };

            debug_assert!(task != self.ready_to_run_queue.stub());

            // Safety:
            // - `task` is a valid pointer.
            // - We are the only thread that accesses the `UnsafeCell` that
            //   contains the stream
            let stream = match unsafe { &mut *(*task).stream.get() } {
                Some(stream) => stream,

                // If the stream has already gone away then we're just
                // cleaning out this task. See the comment in
                // `release_task` for more information, but we're basically
                // just taking ownership of our reference count here.
                None => {
                    // This case only happens when `release_task` was called
                    // for this task before and couldn't drop the task
                    // because it was already enqueued in the ready to run
                    // queue.

                    // Safety: `task` is a valid pointer
                    let task = unsafe { Arc::from_raw(task) };

                    // Double check that the call to `release_task` really
                    // happened. Calling it required the task to be unlinked.
                    debug_assert_eq!(task.next_all.load(Relaxed), self.pending_next_all());
                    unsafe {
                        debug_assert!((*task.prev_all.get()).is_null());
                    }
                    continue;
                }
            };

            // Safety: we only ever access is_done on the thread that owns StreamUnordered.
            if unsafe { *(*task).is_done.get() } {
                // This stream has already been polled to completion.
                // We're keeping it around because the user has not removed it yet.
                // We can ignore any wake-ups for the Stream.
                continue;
            }

            // Safety: `task` is a valid pointer
            let task = unsafe { self.unlink(task) };

            // Unset queued flag: This must be done before polling to ensure
            // that the stream's task gets rescheduled if it sends a wake-up
            // notification **during** the call to `poll`.
            let prev = task.queued.swap(false, SeqCst);
            assert!(prev);

            // We're going to need to be very careful if the `poll`
            // method below panics. We need to (a) not leak memory and
            // (b) ensure that we still don't have any use-after-frees. To
            // manage this we do a few things:
            //
            // * A "bomb" is created which if dropped abnormally will call
            //   `release_task`. That way we'll be sure the memory management
            //   of the `task` is managed correctly. In particular
            //   `release_task` will drop the steam. This ensures that it is
            //   dropped on this thread and not accidentally on a different
            //   thread (bad).
            // * We unlink the task from our internal queue to preemptively
            //   assume it'll panic, in which case we'll want to discard it
            //   regardless.
            struct Bomb<'a, S> {
                queue: &'a mut StreamUnordered<S>,
                task: Option<Arc<Task<S>>>,
            }

            impl<S> Drop for Bomb<'_, S> {
                fn drop(&mut self) {
                    if let Some(task) = self.task.take() {
                        self.queue.release_task(task);
                    }
                }
            }

            let id = task.id;
            let mut bomb = Bomb {
                task: Some(task),
                queue: &mut *self,
            };

            // Poll the underlying stream with the appropriate waker
            // implementation. This is where a large bit of the unsafety
            // starts to stem from internally. The waker is basically just
            // our `Arc<Task<S>>` and can schedule the stream for polling by
            // enqueuing itself in the ready to run queue.
            //
            // Critically though `Task<S>` won't actually access `S`, the
            // stream, while it's floating around inside of wakers.
            // These structs will basically just use `S` to size
            // the internal allocation, appropriately accessing fields and
            // deallocating the task if need be.
            let res = {
                let waker = Task::waker_ref(bomb.task.as_ref().unwrap());
                let mut cx = Context::from_waker(&waker);

                // Safety: We won't move the stream ever again
                let stream = unsafe { Pin::new_unchecked(stream) };

                stream.poll_next(&mut cx)
            };
            polled += 1;

            match res {
                Poll::Pending => {
                    let task = bomb.task.take().unwrap();
                    bomb.queue.link(task);

                    if polled == YIELD_EVERY {
                        // We have polled a large number of futures in a row without yielding.
                        // To ensure we do not starve other tasks waiting on the executor,
                        // we yield here, but immediately wake ourselves up to continue.
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    continue;
                }
                Poll::Ready(None) => {
                    // The stream has completed -- let the user know.
                    // Note that we do not remove the stream here. Instead, we let the user decide
                    // whether to keep the stream for a bit longer, in case they still need to do
                    // some work with it (like if it's also a Sink and they need to flush some more
                    // stuff).

                    // Safe as we only ever access is_done on the thread that owns StreamUnordered.
                    let task = bomb.task.take().unwrap();
                    unsafe {
                        *task.is_done.get() = true;
                    }
                    bomb.queue.link(task);

                    return Poll::Ready(Some((
                        StreamYield::Finished(FinishedStream { token: id }),
                        id,
                    )));
                }
                Poll::Ready(Some(output)) => {
                    // We're not done with the stream just because it yielded something
                    // We're going to need to poll it again!
                    Task::wake_by_ref(bomb.task.as_ref().unwrap());

                    // And also return it to the task queue
                    let task = bomb.task.take().unwrap();
                    bomb.queue.link(task);

                    return Poll::Ready(Some((StreamYield::Item(output), id)));
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl<S> Debug for StreamUnordered<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StreamUnordered {{ ... }}")
    }
}

impl<S> Drop for StreamUnordered<S> {
    fn drop(&mut self) {
        // When a `StreamUnordered` is dropped we want to drop all streams
        // associated with it. At the same time though there may be tons of
        // wakers flying around which contain `Task<S>` references
        // inside them. We'll let those naturally get deallocated.
        unsafe {
            while !self.head_all.get_mut().is_null() {
                let head = *self.head_all.get_mut();
                let task = self.unlink(head);
                self.release_task(task);
            }
        }

        // Note that at this point we could still have a bunch of tasks in the
        // ready to run queue. None of those tasks, however, have streams
        // associated with them so they're safe to destroy on any thread. At
        // this point the `StreamUnordered` struct, the owner of the one strong
        // reference to the ready to run queue will drop the strong reference.
        // At that point whichever thread releases the strong refcount last (be
        // it this thread or some other thread as part of an `upgrade`) will
        // clear out the ready to run queue and free all remaining tasks.
        //
        // While that freeing operation isn't guaranteed to happen here, it's
        // guaranteed to happen "promptly" as no more "blocking work" will
        // happen while there's a strong refcount held.
    }
}

impl<S: Stream> FromIterator<S> for StreamUnordered<S> {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        let acc = StreamUnordered::new();
        iter.into_iter().fold(acc, |mut acc, item| {
            acc.insert(item);
            acc
        })
    }
}

impl<S: Stream> FusedStream for StreamUnordered<S> {
    fn is_terminated(&self) -> bool {
        self.is_terminated.load(Relaxed)
    }
}

#[cfg(test)]
mod micro {
    use super::*;
    use futures_util::{stream, stream::StreamExt};
    use std::pin::Pin;

    #[test]
    fn no_starvation() {
        let forever0 = Box::pin(stream::iter(vec![0].into_iter().cycle()));
        let forever1 = Box::pin(stream::iter(vec![1].into_iter().cycle()));
        let two = Box::pin(stream::iter(vec![2].into_iter()));
        let mut s = StreamUnordered::new();
        let forever0 = s.insert(forever0 as Pin<Box<dyn Stream<Item = i32>>>);
        let forever1 = s.insert(forever1 as Pin<Box<dyn Stream<Item = i32>>>);
        let two = s.insert(two as Pin<Box<dyn Stream<Item = i32>>>);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut s = rt.block_on(s.take(100).collect::<Vec<_>>()).into_iter();
        let mut got_two = false;
        let mut got_two_end = false;
        while let Some((v, si)) = s.next() {
            if let StreamYield::Item(v) = v {
                if si == two {
                    assert_eq!(v, 2);
                    got_two = true;
                } else if si == forever0 {
                    assert_eq!(v, 0);
                } else if si == forever1 {
                    assert_eq!(v, 1);
                } else {
                    unreachable!("unknown stream {} yielded {}", si, v);
                }
            } else if si == two {
                got_two_end = true;
            } else {
                unreachable!("unexpected stream end for stream {}", si);
            }
        }
        assert!(got_two, "stream was starved");
        assert!(got_two_end, "stream end was not announced");
    }
}
