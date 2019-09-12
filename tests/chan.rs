use futures_core::{ready, Stream};
use futures_sink::Sink;
use futures_util::stream::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use streamunordered::*;
use tokio::prelude::*;

async fn echoer<I, RX, TX>(on: I)
where
    I: Stream<Item = (RX, TX)>,
    RX: Stream<String>,
    TX: Stream<String>,
{
    select
}

struct Echoer {
    incoming: tokio::sync::mpsc::Receiver<(
        tokio::sync::mpsc::Receiver<String>,
        tokio::sync::mpsc::Sender<String>,
    )>,
    inputs: StreamUnordered<tokio::sync::mpsc::Receiver<String>>,
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
        while let Poll::Ready(Some((rx, tx))) = Pin::new(&mut self.incoming).poll_next(cx) {
            let slot = self.inputs.stream_slot();
            self.outputs.insert(slot.token(), tx);
            slot.insert(rx);
        }
        Ok(())
    }

    fn try_flush(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Result<(), tokio::sync::mpsc::error::SendError> {
        // start sending new things
        for (&stream, out) in &mut self.out {
            let s = self.outputs.get_mut(&stream).unwrap();
            let mut s = Pin::new(s);
            while !out.is_empty() {
                if let Poll::Pending = s.as_mut().poll_ready(cx)? {
                    break;
                }

                s.as_mut().start_send(out.pop_front().expect("!is_empty"))?;
                self.pending.insert(stream);
            }
        }

        // poll things that are pending
        let mut err = Vec::new();
        let outputs = &mut self.outputs;
        self.pending.retain(|stream| {
            match Pin::new(outputs.get_mut(stream).unwrap()).poll_flush(cx) {
                Poll::Ready(Ok(())) => false,
                Poll::Pending => true,
                Poll::Ready(Err(e)) => {
                    err.push(e);
                    false
                }
            }
        });

        if !err.is_empty() {
            Err(err.swap_remove(0))
        } else {
            Ok(())
        }
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
                Poll::Ready(Some((StreamYield::Finished, _))) => continue,
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
    let (mut mk_tx, mk_rx) = tokio::sync::mpsc::channel(1024);
    tokio::spawn(Echoer::new(mk_rx));

    let (mut tx, remote_rx) = tokio::sync::mpsc::channel(1024);
    let (remote_tx, mut rx) = tokio::sync::mpsc::channel(1024);
    mk_tx.send((remote_rx, remote_tx)).await.unwrap();
    tx.send(String::from("hello world")).await.unwrap();
    let r = rx.next().await.unwrap();
    assert_eq!(r, String::from("hello world"));
}

#[tokio::test]
async fn twoshot() {
    let (mut mk_tx, mk_rx) = tokio::sync::mpsc::channel(1024);
    tokio::spawn(Echoer::new(mk_rx));

    let (mut tx, remote_rx) = tokio::sync::mpsc::channel(1024);
    let (remote_tx, mut rx) = tokio::sync::mpsc::channel(1024);
    mk_tx.send((remote_rx, remote_tx)).await.unwrap();
    tx.send(String::from("hello world")).await.unwrap();
    let r = rx.next().await.unwrap();
    assert_eq!(r, String::from("hello world"));
    tx.send(String::from("goodbye world")).await.unwrap();
    let r = rx.next().await.unwrap();
    assert_eq!(r, String::from("goodbye world"));
}
