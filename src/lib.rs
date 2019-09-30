//! Use an [async closure][async] to produce (yield) items for a stream.
//!
//! Example:
//!
//! ```rust text
//! use futures::StreamExt;
//! use futures::executor::block_on;
//! use async_stream::AsyncStream;
//!
//! let mut strm = AsyncStream::<u8, std::io::Error>::new(move |mut y| async move {
//!     for i in 0u8..10 {
//!         y.send(i).await;
//!     }
//!     Ok(())
//! });
//!
//! let fut = async {
//!     let mut count = 0;
//!     while let Some(item) = strm.next().await {
//!         println!("{:?}", item);
//!         count += 1;
//!     }
//!     assert!(count == 10);
//! };
//! block_on(fut);
//!
//! ```
//!
//! The stream will produce an `Item/Error` (for [0.1 streams][Stream01])
//! or a `Result<Item, Error>` (for [0.3 streams][Stream03]) where the `Item`
//! is an item sent with [tx.send(item)][send]. Any errors returned by
//! the async closure will be returned as an error value on
//! the stream.
//!
//! On success the async closure should return `Ok(())`.
//!
//! [async]: https://rust-lang.github.io/async-book/getting_started/async_await_primer.html
//! [Stream01]: https://docs.rs/futures/0.1/futures/stream/trait.Stream.html
//! [Stream03]: https://rust-lang-nursery.github.io/futures-api-docs/0.3.0-alpha.16/futures/stream/trait.Stream.html
//! [send]: async_stream/struct.Sender.html#method.send
//!
use std::cell::Cell;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use futures::task::Context;
use futures::task::Poll as Poll03;
use futures::Future as Future03;
use futures::Stream as Stream03;

/// Future returned by the Sender.send() method.
///
/// Completes when the item is sent.
#[must_use]
pub struct SenderFuture {
    is_ready: bool,
}

impl SenderFuture {
    fn new() -> SenderFuture {
        SenderFuture { is_ready: false }
    }
}

impl Future03 for SenderFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll03<Self::Output> {
        if self.is_ready {
            Poll03::Ready(())
        } else {
            self.is_ready = true;
            Poll03::Pending
        }
    }
}

// Only internally used by one AsyncStream and never shared
// in any other way, so we don't have to use Arc<Mutex<..>>.
/// Type of the sender passed as first argument into the async closure.
pub struct Sender<I, E>(Arc<Cell<Option<I>>>, PhantomData<E>);
unsafe impl<I, E> Sync for Sender<I, E> {}
unsafe impl<I, E> Send for Sender<I, E> {}

impl<I, E> Sender<I, E> {
    fn new(item_opt: Option<I>) -> Sender<I, E> {
        Sender(Arc::new(Cell::new(item_opt)), PhantomData::<E>)
    }

    // note that this is NOT impl Clone for Sender, it's private.
    fn clone(&self) -> Sender<I, E> {
        Sender(self.0.clone(), PhantomData::<E>)
    }

    /// Send one item to the stream.
    pub fn send<T>(&mut self, item: T) -> SenderFuture
    where
        T: Into<I>,
    {
        self.0.set(Some(item.into()));
        SenderFuture::new()
    }
}

/// An abstraction around a future, where the
/// future can internally loop and yield items.
///
/// AsyncStream::new() takes a [futures 0.3 Future][Future03] ([async closure][async], usually)
/// and AsyncStream then implements both a [futures 0.1 Stream][Stream01] and a
/// [futures 0.3 Stream][Stream03].
///
/// [async]: https://rust-lang.github.io/async-book/getting_started/async_await_primer.html
/// [Future03]: https://doc.rust-lang.org/nightly/std/future/trait.Future.html
/// [Stream01]: https://docs.rs/futures/0.1/futures/stream/trait.Stream.html
/// [Stream03]: https://rust-lang-nursery.github.io/futures-api-docs/0.3.0-alpha.16/futures/stream/trait.Stream.html
#[must_use]
pub struct AsyncStream<Item, Error> {
    item: Sender<Item, Error>,
    fut: Option<Pin<Box<dyn Future03<Output = Result<(), Error>> + 'static + Send>>>,
}

impl<Item, Error: 'static + Send> AsyncStream<Item, Error> {
    /// Create a new stream from a closure returning a Future 0.3,
    /// or an "async closure" (which is the same).
    ///
    /// The closure is passed one argument, the sender, which has a
    /// method "send" that can be called to send a item to the stream.
    ///
    /// The AsyncStream instance that is returned impl's both
    /// a futures 0.1 Stream and a futures 0.3 Stream.
    pub fn new<F, R>(f: F) -> Self
    where
        F: FnOnce(Sender<Item, Error>) -> R,
        R: Future03<Output = Result<(), Error>> + Send + 'static,
        Item: 'static,
    {
        let sender = Sender::new(None);
        AsyncStream::<Item, Error> {
            item: sender.clone(),
            fut: Some(Box::pin(f(sender))),
        }
    }
}

#[cfg(feature = "compat")]
pub mod compat {
    use futures::compat::Compat as Compat03As01;
    use futures01::Async as Async01;
    use futures01::Future as Future01;
    use futures01::Stream as Stream01;

    /// Stream implementation for Futures 0.1.
    impl<I, E> Stream01 for AsyncStream<I, E> {
        type Item = I;
        type Error = E;

        fn poll(&mut self) -> Result<Async01<Option<Self::Item>>, Self::Error> {
            // We use a futures::compat::Compat wrapper to be able to call
            // the futures 0.3 Future in a futures 0.1 context. Because
            // the Compat wrapper wants to to take ownership, the future
            // is stored in an Option which we can temporarily move it out
            // of, and then move it back in.
            let mut fut = Compat03As01::new(self.fut.take().unwrap());
            let pollres = fut.poll();
            self.fut.replace(fut.into_inner());
            match pollres {
                Ok(Async01::Ready(_)) => Ok(Async01::Ready(None)),
                Ok(Async01::NotReady) => {
                    let mut item = self.item.0.replace(None);
                    if item.is_none() {
                        Ok(Async01::NotReady)
                    } else {
                        Ok(Async01::Ready(item.take()))
                    }
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// Stream implementation for Futures 0.3.
impl<I, E: Unpin> Stream03 for AsyncStream<I, E> {
    type Item = Result<I, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll03<Option<Result<I, E>>> {
        let pollres = {
            let fut = self.fut.as_mut().unwrap();
            fut.as_mut().poll(cx)
        };
        match pollres {
            // If the future returned Poll::Ready, that signals the end of the stream.
            Poll03::Ready(Ok(_)) => Poll03::Ready(None),
            Poll03::Ready(Err(e)) => Poll03::Ready(Some(Err(e))),
            Poll03::Pending => {
                // Pending means that some sub-future returned pending. That sub-future
                // _might_ have been the SenderFuture returned by Sender.send, so
                // check if there is an item available in self.item.
                let mut item = self.item.0.replace(None);
                if item.is_none() {
                    Poll03::Pending
                } else {
                    Poll03::Ready(Some(Ok(item.take().unwrap())))
                }
            }
        }
    }
}
