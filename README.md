# streamunordered

[![Crates.io](https://img.shields.io/crates/v/streamunordered.svg)](https://crates.io/crates/streamunordered)
[![Documentation](https://docs.rs/streamunordered/badge.svg)](https://docs.rs/streamunordered/)
[![Build Status](https://travis-ci.org/jonhoo/streamunordered.svg?branch=master)](https://travis-ci.org/jonhoo/streamunordered)

A stream that efficiently multiplexes multiple streams.

This "combinator" provides the ability to maintain and drive a set of streams to completion,
while also providing access to each stream as it yields new elements.

Streams are pushed into this set and their realized values are yielded as they are produced.
This structure is optimized to manage a large number of streams. Streams managed by
`StreamUnordered` will only be polled when they generate notifications. This reduces the
required amount of work needed to coordinate large numbers of streams.

When a `StreamUnordered` is first created, it does not contain any streams. Calling `poll` in
this state will result in `Ok(Async::Ready(None))` to be returned. Streams are submitted to the
set using `push`; however, the stream will **not** be polled at this point. `StreamUnordered`
will only poll managed streams when `StreamUnordered::poll` is called. As such, it is important
to call `poll` after pushing new streams.

If `StreamUnordered::poll` returns `Ok(Async::Ready(None))` this means that the set is
currently not managing any streams. A stream may be submitted to the set at a later time. At
that point, a call to `StreamUnordered::poll` will either return the stream's resolved value
**or** `Ok(Async::NotReady)` if the stream has not yet completed.

Whenever a value is yielded, the yielding stream's index is also included. A reference to the
stream that originated the value is obtained by using [`StreamUnordered::get`] or
[`StreamUnordered::get_mut`].
