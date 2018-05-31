extern crate async_bincode;
extern crate bincode;
extern crate futures;
extern crate streamunordered;
extern crate tokio;

use std::collections::{HashMap, HashSet, VecDeque};

use streamunordered::*;
use tokio::prelude::*;

struct Echoer {
    incoming: futures::sync::mpsc::Receiver<(
        futures::sync::mpsc::Receiver<String>,
        futures::sync::mpsc::Sender<String>,
    )>,
    inputs: StreamUnordered<futures::sync::mpsc::Receiver<String>>,
    outputs: HashMap<usize, futures::sync::mpsc::Sender<String>>,
    out: HashMap<usize, VecDeque<String>>,
    pending: HashSet<usize>,
}

impl Echoer {
    pub fn new(
        on: futures::sync::mpsc::Receiver<(
            futures::sync::mpsc::Receiver<String>,
            futures::sync::mpsc::Sender<String>,
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

    fn try_new(&mut self) -> Result<(), ()> {
        'more: while let Async::Ready(Some((rx, tx))) = self.incoming.poll()? {
            let slot = self.inputs.stream_slot();
            self.outputs.insert(slot.token(), tx);
            slot.insert(rx);
        }
        Ok(())
    }

    fn try_flush(&mut self) -> Result<(), futures::sync::mpsc::SendError<String>> {
        // start sending new things
        for (&stream, out) in &mut self.out {
            let s = self.outputs.get_mut(&stream).unwrap();
            while let Some(x) = out.pop_front() {
                match s.start_send(x)? {
                    AsyncSink::Ready => {
                        self.pending.insert(stream);
                    }
                    AsyncSink::NotReady(x) => {
                        out.push_front(x);
                        break;
                    }
                }
            }
        }

        // poll things that are pending
        let mut err = Vec::new();
        let outputs = &mut self.outputs;
        self.pending.retain(
            |stream| match outputs.get_mut(stream).unwrap().poll_complete() {
                Ok(Async::Ready(())) => false,
                Ok(Async::NotReady) => true,
                Err(e) => {
                    err.push(e);
                    false
                }
            },
        );

        if !err.is_empty() {
            Err(err.swap_remove(0))
        } else {
            Ok(())
        }
    }
}

impl Future for Echoer {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        // see if there are any new connections
        self.try_new().unwrap();

        // see if there's new input for us
        loop {
            match self.inputs.poll().unwrap() {
                Async::Ready(Some((packet, sender))) => {
                    self.out
                        .entry(sender)
                        .or_insert_with(VecDeque::new)
                        .push_back(packet);
                }
                Async::Ready(None) => {
                    // no connections yet
                    break;
                }
                Async::NotReady => break,
            }
        }

        // send stuff that needs to be sent
        self.try_flush().unwrap();

        Ok(Async::NotReady)
    }
}

#[test]
fn oneshot() {
    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let (mk_tx, mk_rx) = futures::sync::mpsc::channel(1024);
    rt.spawn(Echoer::new(mk_rx));

    let (tx, remote_rx) = futures::sync::mpsc::channel(1024);
    let (remote_tx, rx) = futures::sync::mpsc::channel(1024);
    let ping = mk_tx
        .send((remote_rx, remote_tx))
        .map_err(|e| panic!("{:?}", e))
        .and_then(move |_| tx.send(String::from("hello world")))
        .map_err(|e| panic!("{:?}", e))
        .and_then(move |tx| rx.into_future().map(|(r, rx)| (r, rx, tx)))
        .map_err(|e| panic!("{:?}", e))
        .map(|(r, rx, tx)| {
            assert_eq!(r, Some(String::from("hello world")));
            (rx, tx)
        })
        .map(|_| ());

    ping.wait().unwrap();
}

#[test]
fn twoshot() {
    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let (mk_tx, mk_rx) = futures::sync::mpsc::channel(1024);
    rt.spawn(Echoer::new(mk_rx));

    let (tx, remote_rx) = futures::sync::mpsc::channel(1024);
    let (remote_tx, rx) = futures::sync::mpsc::channel(1024);
    let ping = mk_tx
        .send((remote_rx, remote_tx))
        .map_err(|e| panic!("{:?}", e))
        .and_then(move |_| tx.send(String::from("hello world")))
        .map_err(|e| panic!("{:?}", e))
        .and_then(move |tx| rx.into_future().map(|(r, rx)| (r, rx, tx)))
        .map_err(|e| panic!("{:?}", e))
        .map(|(r, rx, tx)| {
            assert_eq!(r, Some(String::from("hello world")));
            (rx, tx)
        })
        .and_then(|(rx, tx)| {
            tx.send(String::from("goodbye world"))
                .map(move |tx| (rx, tx))
        })
        .map_err(|e| panic!("{:?}", e))
        .and_then(|(rx, tx)| rx.into_future().map(move |(r, rx)| (r, rx, tx)))
        .map_err(|e| panic!("{:?}", e))
        .map(|(r, rx, tx)| {
            assert_eq!(r, Some(String::from("goodbye world")));
            (rx, tx)
        })
        .map(|_| ());

    ping.wait().unwrap();
}
