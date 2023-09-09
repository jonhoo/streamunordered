use std::{future::ready, pin::pin};

use futures_util::stream;
use streamunordered::StreamUnordered;

#[test]
fn iter_methods() {
    let mut streams = StreamUnordered::new();
    for i in 0..5 {
        streams.insert(stream::once(ready(i)));
    }

    let mut result: (Vec<_>, Vec<_>) = streams.iter_with_token().unzip();
    result.1.sort(); // We sort as order is not guaranteed.
    assert_eq!(result.1, vec![1, 2, 3, 4, 5]);

    assert_eq!(streams.iter_mut().len(), 5);

    let mut result: (Vec<_>, Vec<_>) = streams.iter_mut_with_token().unzip();
    result.1.sort(); // We sort as order is not guaranteed.
    assert_eq!(result.1, vec![1, 2, 3, 4, 5]);

    let mut streams = pin!(streams);

    assert_eq!(streams.as_mut().iter_pin_mut().len(), 5);

    let mut result: (Vec<_>, Vec<_>) = streams.iter_pin_mut_with_token().unzip();
    result.1.sort(); // We sort as order is not guaranteed.
    assert_eq!(result.1, vec![1, 2, 3, 4, 5]);
}
