use async_bincode::*;
use futures_core::Stream;
use futures_sink::Sink;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use streamunordered::*;
use tokio::prelude::*;

struct Echoer<S> {
    incoming: S,
    inputs: StreamUnordered<
        AsyncBincodeStream<tokio::net::TcpStream, String, String, AsyncDestination>,
    >,
    out: HashMap<usize, VecDeque<String>>,
    pending: HashSet<usize>,
}

impl<S> Echoer<S>
where
    S: Stream<Item = io::Result<tokio::net::TcpStream>> + Unpin,
{
    pub fn new(on: S) -> Self {
        Echoer {
            incoming: on,
            inputs: Default::default(),
            out: Default::default(),
            pending: Default::default(),
        }
    }

    fn try_new(&mut self, cx: &mut Context<'_>) -> bincode::Result<()> {
        while let Poll::Ready(Some(stream)) = Pin::new(&mut self.incoming).poll_next(cx)? {
            let slot = self.inputs.stream_entry();
            let tcp = AsyncBincodeStream::from(stream).for_async();
            slot.insert(tcp);
        }
        Ok(())
    }

    fn try_flush(&mut self, cx: &mut Context<'_>) -> bincode::Result<()> {
        // start sending new things
        for (&stream, out) in &mut self.out {
            let s = &mut self.inputs[stream];
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
        let inputs = &mut self.inputs;
        self.pending.retain(
            |&stream| match Pin::new(&mut inputs[stream]).poll_flush(cx) {
                Poll::Ready(Ok(())) => false,
                Poll::Pending => true,
                Poll::Ready(Err(e)) => {
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

impl<S> Future for Echoer<S>
where
    S: Stream<Item = io::Result<tokio::net::TcpStream>> + Unpin,
{
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
                        .push_back(packet.unwrap());
                }
                Poll::Ready(Some((StreamYield::Finished, _))) => continue,
                Poll::Ready(None) => {
                    // no connections yet
                    break;
                }
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
    let on = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = on.local_addr().unwrap();
    tokio::spawn(Echoer::new(on.incoming()));

    let s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let mut s = AsyncBincodeStream::from(s).for_async();
    s.send(String::from("hello world")).await.unwrap();
    let r: String = s.next().await.unwrap().unwrap();
    assert_eq!(r, String::from("hello world"));
}

#[tokio::test]
async fn twoshot() {
    let on = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = on.local_addr().unwrap();
    tokio::spawn(Echoer::new(on.incoming()));

    let s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let mut s = AsyncBincodeStream::from(s).for_async();
    s.send(String::from("hello world")).await.unwrap();
    let r: String = s.next().await.unwrap().unwrap();
    assert_eq!(r, String::from("hello world"));
    s.send(String::from("goodbye world")).await.unwrap();
    let r = s.next().await.unwrap().unwrap();
    assert_eq!(r, String::from("goodbye world"));
}
