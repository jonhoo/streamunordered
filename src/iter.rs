use super::task::Task;
use super::StreamUnordered;
use core::marker::PhantomData;
use core::pin::Pin;

#[derive(Debug)]
/// Mutable iterator over all streams in the unordered set.
pub struct IterPinMut<'a, S> {
    pub(super) task: *const Task<S>,
    pub(super) len: usize,
    pub(super) _marker: PhantomData<&'a mut StreamUnordered<S>>,
}

#[derive(Debug)]
/// Mutable iterator over all streams in the unordered set.
pub struct IterMut<'a, S: Unpin>(pub(super) IterPinMut<'a, S>);

impl<'a, S> Iterator for IterPinMut<'a, S> {
    type Item = Pin<&'a mut S>;

    fn next(&mut self) -> Option<Pin<&'a mut S>> {
        if self.task.is_null() {
            return None;
        }
        unsafe {
            let stream = (*(*self.task).stream.get()).as_mut().unwrap();
            let next = *(*self.task).next_all.get();
            self.task = next;
            self.len -= 1;
            Some(Pin::new_unchecked(stream))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<S> ExactSizeIterator for IterPinMut<'_, S> {}

impl<'a, S: Unpin> Iterator for IterMut<'a, S> {
    type Item = &'a mut S;

    fn next(&mut self) -> Option<&'a mut S> {
        self.0.next().map(Pin::get_mut)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<S: Unpin> ExactSizeIterator for IterMut<'_, S> {}
