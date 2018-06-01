extern crate async_bincode;
extern crate bincode;
extern crate futures;
extern crate streamunordered;
extern crate tokio;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;

use async_bincode::*;
use streamunordered::*;
use tokio::prelude::*;

struct Echoer {
    incoming: tokio::net::Incoming,
    inputs: StreamUnordered<
        AsyncBincodeStream<tokio::net::TcpStream, String, String, AsyncDestination>,
    >,
    out: HashMap<usize, VecDeque<String>>,
    pending: HashSet<usize>,
}

impl Echoer {
    pub fn new(on: tokio::net::TcpListener) -> Self {
        Echoer {
            incoming: on.incoming(),
            inputs: Default::default(),
            out: Default::default(),
            pending: Default::default(),
        }
    }

    fn try_new(&mut self) -> bincode::Result<()> {
        'more: while let Async::Ready(Some(stream)) = self.incoming.poll()? {
            let slot = self.inputs.stream_slot();
            let tcp = AsyncBincodeStream::from(stream).for_async();
            slot.insert(tcp);
        }
        Ok(())
    }

    fn try_flush(&mut self) -> bincode::Result<()> {
        // start sending new things
        for (&stream, out) in &mut self.out {
            let s = &mut self.inputs[stream];
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
        let inputs = &mut self.inputs;
        self.pending
            .retain(|&stream| match inputs[stream].poll_complete() {
                Ok(Async::Ready(())) => false,
                Ok(Async::NotReady) => true,
                Err(e) => {
                    err.push(e);
                    false
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
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        let res = (|| -> Result<Async<Self::Item>, bincode::Error> {
            // see if there are any new connections
            self.try_new()?;

            // see if there's new input for us
            loop {
                match self.inputs.poll()? {
                    Async::Ready(Some((StreamYield::Item(packet), sender))) => {
                        self.out
                            .entry(sender)
                            .or_insert_with(VecDeque::new)
                            .push_back(packet);
                    }
                    Async::Ready(Some((StreamYield::Finished(..), _))) => continue,
                    Async::Ready(None) => {
                        // no connections yet
                        break;
                    }
                    Async::NotReady => break,
                }
            }

            // send stuff that needs to be sent
            self.try_flush()?;

            Ok(Async::NotReady)
        })();

        res.map_err(|_| ())
    }
}

#[test]
fn oneshot() {
    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let on =
        tokio::net::TcpListener::bind(&SocketAddr::new("127.0.0.1".parse().unwrap(), 0)).unwrap();
    let addr = on.local_addr().unwrap();
    rt.spawn(Echoer::new(on));

    let ping = tokio::net::TcpStream::connect(&addr)
        .map(AsyncBincodeStream::from)
        .map(AsyncBincodeStream::for_async)
        .map_err(|e| panic!("{:?}", e))
        .and_then(|s| s.send(String::from("hello world")))
        .map_err(|e| panic!("{:?}", e))
        .and_then(|s| s.into_future())
        .map_err(|e| panic!("{:?}", e))
        .map(|(r, s)| {
            assert_eq!(r, Some(String::from("hello world")));
            s
        })
        .map(|_| ());

    ping.wait().unwrap();
}

#[test]
fn twoshot() {
    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let on =
        tokio::net::TcpListener::bind(&SocketAddr::new("127.0.0.1".parse().unwrap(), 0)).unwrap();
    let addr = on.local_addr().unwrap();
    rt.spawn(Echoer::new(on));

    let ping = tokio::net::TcpStream::connect(&addr)
        .map(AsyncBincodeStream::from)
        .map(AsyncBincodeStream::for_async)
        .map_err(|e| panic!("{:?}", e))
        .and_then(|s| s.send(String::from("hello world")))
        .map_err(|e| panic!("{:?}", e))
        .and_then(|s| s.into_future())
        .map_err(|e| panic!("{:?}", e))
        .map(|(r, s)| {
            assert_eq!(r, Some(String::from("hello world")));
            s
        })
        .and_then(|s| s.send(String::from("goodbye world")))
        .map_err(|e| panic!("{:?}", e))
        .and_then(|s| s.into_future())
        .map_err(|e| panic!("{:?}", e))
        .map(|(r, s)| {
            assert_eq!(r, Some(String::from("goodbye world")));
            s
        })
        .map(|_| ());

    ping.wait().unwrap();
}
