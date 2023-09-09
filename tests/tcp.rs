use async_bincode::{AsyncBincodeStream, AsyncDestination};
use futures::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use streamunordered::*;

struct Echoer {
    incoming: tokio::net::TcpListener,
    inputs: StreamUnordered<
        AsyncBincodeStream<tokio::net::TcpStream, String, String, AsyncDestination>,
    >,
    out: HashMap<usize, VecDeque<String>>,
    pending: HashSet<usize>,
}

impl Echoer {
    pub fn new(on: tokio::net::TcpListener) -> Self {
        Echoer {
            incoming: on,
            inputs: Default::default(),
            out: Default::default(),
            pending: Default::default(),
        }
    }

    fn try_new(&mut self, cx: &mut Context<'_>) -> bincode::Result<()> {
        while let Poll::Ready((stream, _)) = self.incoming.poll_accept(cx)? {
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
                Poll::Ready(Ok(())) => {
                    if inputs.is_finished(stream).unwrap_or(false) {
                        // _now_ we can drop the stream
                        Pin::new(&mut *inputs).remove(stream);
                    }
                    false
                }
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
                        .push_back(packet.unwrap());
                }
                Poll::Ready(Some((StreamYield::Finished(f), _))) => {
                    f.keep();
                    continue;
                }
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
    tokio::spawn(Echoer::new(on));

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
    tokio::spawn(Echoer::new(on));

    let s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let mut s = AsyncBincodeStream::from(s).for_async();
    s.send(String::from("hello world")).await.unwrap();
    let r: String = s.next().await.unwrap().unwrap();
    assert_eq!(r, String::from("hello world"));
    s.send(String::from("goodbye world")).await.unwrap();
    let r = s.next().await.unwrap().unwrap();
    assert_eq!(r, String::from("goodbye world"));
}

#[tokio::test]
async fn close_early() {
    let on = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = on.local_addr().unwrap();
    tokio::spawn(Echoer::new(on));

    let s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let mut s = AsyncBincodeStream::from(s).for_async();
    let (mut r, mut w) = s.tcp_split();
    w.send(String::from("hello world")).await.unwrap();
    w.close().await.unwrap();
    let r: String = r.next().await.unwrap().unwrap();
    assert_eq!(r, String::from("hello world"));
}
