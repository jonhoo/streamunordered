use futures::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use streamunordered::*;

struct Echoer {
    incoming: tokio::sync::mpsc::Receiver<(
        tokio::sync::mpsc::Receiver<String>,
        tokio::sync::mpsc::Sender<String>,
    )>,
    inputs: StreamUnordered<tokio_stream::wrappers::ReceiverStream<String>>,
    outputs: HashMap<usize, tokio::sync::mpsc::Sender<String>>,
    out: HashMap<usize, VecDeque<String>>,
    pending: HashSet<usize>,
}

impl Echoer {
    pub fn new(
        on: tokio::sync::mpsc::Receiver<(
            tokio::sync::mpsc::Receiver<String>,
            tokio::sync::mpsc::Sender<String>,
        )>,
    ) -> Self {
        Echoer {
            incoming: on,
            inputs: Default::default(),
            outputs: Default::default(),
            out: Default::default(),
            pending: Default::default(),
        }
    }

    fn try_new(&mut self, cx: &mut Context<'_>) -> Result<(), ()> {
        while let Poll::Ready(Some((rx, tx))) = self.incoming.poll_recv(cx) {
            let slot = self.inputs.stream_entry();
            self.outputs.insert(slot.token(), tx);
            slot.insert(tokio_stream::wrappers::ReceiverStream::new(rx));
        }
        Ok(())
    }

    fn try_flush(&mut self, _: &mut Context<'_>) -> Result<(), ()> {
        // start sending new things
        for (&stream, out) in &mut self.out {
            let s = self.outputs.get_mut(&stream).unwrap();
            while !out.is_empty() {
                use tokio::sync::mpsc::error::TrySendError;
                match s.try_reserve() {
                    Err(TrySendError::Full(_)) => {
                        // NOTE: This is likely wrong -- we need poll_reserve so that we'll be
                        // woken up again when the channel _does_ have capacity.
                        break;
                    }
                    Err(TrySendError::Closed(_)) => return Err(()),
                    Ok(p) => {
                        p.send(out.pop_front().expect("!is_empty"));
                        self.pending.insert(stream);
                    }
                }
            }
        }

        // NOTE: no need to flush channels

        Ok(())
    }
}

impl Future for Echoer {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // see if there are any new connections
        self.try_new(cx).unwrap();

        // see if there's new input for us
        loop {
            match Pin::new(&mut self.inputs).poll_next(cx) {
                Poll::Ready(Some((StreamYield::Item(packet), sender))) => {
                    self.out
                        .entry(sender)
                        .or_insert_with(VecDeque::new)
                        .push_back(packet);
                }
                Poll::Ready(Some((StreamYield::Finished(f), _))) => {
                    f.remove(Pin::new(&mut self.inputs));
                    continue;
                }
                Poll::Ready(None) => unreachable!(),
                Poll::Pending => break,
            }
        }

        // send stuff that needs to be sent
        self.try_flush(cx).unwrap();

        Poll::Pending
    }
}

#[tokio::test]
async fn oneshot() {
    let (mk_tx, mk_rx) = tokio::sync::mpsc::channel(1024);
    tokio::spawn(Echoer::new(mk_rx));

    let (tx, remote_rx) = tokio::sync::mpsc::channel(1024);
    let (remote_tx, mut rx) = tokio::sync::mpsc::channel(1024);
    mk_tx.send((remote_rx, remote_tx)).await.unwrap();
    tx.send(String::from("hello world")).await.unwrap();
    let r = rx.recv().await.unwrap();
    assert_eq!(r, String::from("hello world"));
}

#[tokio::test]
async fn twoshot() {
    let (mk_tx, mk_rx) = tokio::sync::mpsc::channel(1024);
    tokio::spawn(Echoer::new(mk_rx));

    let (tx, remote_rx) = tokio::sync::mpsc::channel(1024);
    let (remote_tx, mut rx) = tokio::sync::mpsc::channel(1024);
    mk_tx.send((remote_rx, remote_tx)).await.unwrap();
    tx.send(String::from("hello world")).await.unwrap();
    let r = rx.recv().await.unwrap();
    assert_eq!(r, String::from("hello world"));
    tx.send(String::from("goodbye world")).await.unwrap();
    let r = rx.recv().await.unwrap();
    assert_eq!(r, String::from("goodbye world"));
}
