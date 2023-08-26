use super::task::Task;
use super::StreamUnordered;
use core::marker::PhantomData;
use core::pin::Pin;
use core::sync::atomic::Ordering::Relaxed;

/// Mutable iterator over all streams in the unordered set.
#[derive(Debug)]
pub struct IterPinMutWithToken<'a, S> {
    pub(super) task: *const Task<S>,
    pub(super) len: usize,
    pub(super) _marker: PhantomData<&'a mut StreamUnordered<S>>,
}

/// Mutable iterator over all streams in the unordered set.
#[derive(Debug)]
pub struct IterPinMut<'a, S>(pub(super) IterPinMutWithToken<'a, S>);

/// Mutable iterator over all streams in the unordered set.
#[derive(Debug)]
pub struct IterMutWithToken<'a, S: Unpin>(pub(super) IterPinMutWithToken<'a, S>);

/// Mutable iterator over all streams in the unordered set.
#[derive(Debug)]
pub struct IterMut<'a, S: Unpin>(pub(super) IterPinMutWithToken<'a, S>);

/// Immutable iterator over all streams in the unordered set.
#[derive(Debug)]
pub struct IterWithToken<'a, S> {
    pub(super) task: *const Task<S>,
    pub(super) len: usize,
    pub(super) pending_next_all: *mut Task<S>,
    pub(super) _marker: PhantomData<&'a StreamUnordered<S>>,
}

impl<'a, S> Iterator for IterPinMutWithToken<'a, S> {
    type Item = (Pin<&'a mut S>, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.task.is_null() {
            return None;
        }
        unsafe {
            let id = (*self.task).id;
            let stream = (*(*self.task).stream.get()).as_mut().unwrap();

            // Mutable access to a previously shared `StreamUnordered` implies
            // that the other threads already released the object before the
            // current thread acquired it, so relaxed ordering can be used and
            // valid `next_all` checks can be skipped.
            let next = (*self.task).next_all.load(Relaxed);
            self.task = next;
            self.len -= 1;
            Some((Pin::new_unchecked(stream), id))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<S> ExactSizeIterator for IterPinMutWithToken<'_, S> {}

impl<'a, S> Iterator for IterPinMut<'a, S> {
    type Item = Pin<&'a mut S>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(s, _)| s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<S> ExactSizeIterator for IterPinMut<'_, S> {}

impl<'a, S: Unpin> Iterator for IterMut<'a, S> {
    type Item = &'a mut S;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(stream, _)| Pin::get_mut(stream))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<S: Unpin> ExactSizeIterator for IterMut<'_, S> {}

impl<'a, S: Unpin> Iterator for IterMutWithToken<'a, S> {
    type Item = (&'a mut S, usize);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(stream, id)| (Pin::get_mut(stream), id))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<S: Unpin> ExactSizeIterator for IterMutWithToken<'_, S> {}

impl<'a, S> Iterator for IterWithToken<'a, S> {
    type Item = (&'a S, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.task.is_null() {
            return None;
        }
        unsafe {
            let id = (*self.task).id;
            let stream = (*(*self.task).stream.get()).as_ref().unwrap();

            // Relaxed ordering can be used since acquire ordering when
            // `head_all` was initially read for this iterator implies acquire
            // ordering for all previously inserted nodes (and we don't need to
            // read `len_all` again for any other nodes).
            let next = (*self.task).spin_next_all(self.pending_next_all, Relaxed);
            self.task = next;
            self.len -= 1;
            Some((stream, id))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<S> ExactSizeIterator for IterWithToken<'_, S> {}
