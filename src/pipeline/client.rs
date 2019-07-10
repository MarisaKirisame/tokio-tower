use crate::mediator;
use crate::wrappers::*;
use crate::Error;
use crate::MakeTransport;
use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::{atomic, Arc};
use std::{error, fmt};
use tower_service::Service;

#[cfg(feature = "tracing")]
use tracing::Level;

/// A factory that makes new [`Client`] instances by creating new transports and wrapping them in
/// fresh `Client`s.
pub struct Maker<NT, Request> {
    t_maker: NT,
    _req: PhantomData<fn(Request)>,
}

impl<NT, Request> Maker<NT, Request> {
    /// Make a new `Client` factory that uses the given `MakeTransport` factory.
    pub fn new(t: NT) -> Self {
        Maker {
            t_maker: t,
            _req: PhantomData,
        }
    }

    // NOTE: it'd be *great* if the user had a way to specify a service error handler for all
    // spawned services, but without https://github.com/rust-lang/rust/pull/49224 or
    // https://github.com/rust-lang/rust/issues/29625 that's pretty tricky (unless we're willing to
    // require Fn + Clone)
}

/// A `Future` that will resolve into a `Client<T::Transport>`.
pub struct NewSpawnedClientFuture<NT, Target, Request>
where
    NT: MakeTransport<Target, Request>,
{
    maker: Option<NT::Future>,
}

/// A failure to spawn a new `Client`.
#[derive(Debug)]
pub enum SpawnError<E> {
    /// The executor failed to spawn the `tower_buffer::Worker`.
    SpawnFailed,

    /// A new transport could not be produced.
    Inner(E),
}

impl<NT, Target, Request> Future for NewSpawnedClientFuture<NT, Target, Request>
where
    NT: MakeTransport<Target, Request>,
    NT::Transport: 'static + Send,
    Request: 'static + Send,
    NT::Item: 'static + Send,
    NT::SinkError: 'static + Send + Sync,
    NT::Error: 'static + Send + Sync,
{
    type Item = Client<NT::Transport, Error<NT::Transport>>;
    type Error = SpawnError<NT::MakeError>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.maker.take() {
            None => unreachable!("poll called after future resolved"),
            Some(mut fut) => match fut.poll().map_err(SpawnError::Inner)? {
                Async::Ready(t) => Ok(Async::Ready(Client::new(t))),
                Async::NotReady => {
                    self.maker = Some(fut);
                    Ok(Async::NotReady)
                }
            },
        }
    }
}

impl<NT, Target, Request> Service<Target> for Maker<NT, Request>
where
    NT: MakeTransport<Target, Request>,
    NT::Transport: 'static + Send,
    Request: 'static + Send,
    NT::Item: 'static + Send,
    NT::SinkError: 'static + Send + Sync,
    NT::Error: 'static + Send + Sync,
{
    type Error = SpawnError<NT::MakeError>;
    type Response = Client<NT::Transport, Error<NT::Transport>>;
    type Future = NewSpawnedClientFuture<NT, Target, Request>;

    fn call(&mut self, target: Target) -> Self::Future {
        NewSpawnedClientFuture {
            maker: Some(self.t_maker.make_transport(target)),
        }
    }

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.t_maker.poll_ready().map_err(SpawnError::Inner)
    }
}

impl<NT, Request> tower_load::Load for Maker<NT, Request> {
    type Metric = u8;

    fn load(&self) -> Self::Metric {
        0
    }
}

/// This type provides an implementation of a Tower
/// [`Service`](https://docs.rs/tokio-service/0.1/tokio_service/trait.Service.html) on top of a
/// request-at-a-time protocol transport. In particular, it wraps a transport that implements
/// `Sink<SinkItem = Request>` and `Stream<Item = Response>` with the necessary bookkeeping to
/// adhere to Tower's convenient `fn(Request) -> Future<Response>` API.
pub struct Client<T, E>
where
    T: Sink + Stream,
{
    mediator: mediator::Sender<ClientRequest<T>>,
    in_flight: Arc<atomic::AtomicUsize>,
    _error: PhantomData<fn(E)>,
}

struct Pending<Item> {
    tx: tokio_sync::oneshot::Sender<ClientResponse<Item>>,
    #[cfg(feature = "tracing")]
    span: tracing::Span,
}

struct ClientInner<T, E>
where
    T: Sink + Stream,
{
    mediator: mediator::Receiver<ClientRequest<T>>,
    responses: VecDeque<Pending<T::Item>>,
    waiting: Option<ClientRequest<T>>,
    transport: T,

    in_flight: Arc<atomic::AtomicUsize>,
    finish: bool,

    #[allow(unused)]
    error: PhantomData<fn(E)>,
}

impl<T, E> Client<T, E>
where
    T: Sink + Stream + Send + 'static,
    E: From<Error<T>>,
    E: 'static + Send,
    T::SinkItem: 'static + Send,
    T::Item: 'static + Send,
{
    /// Construct a new [`Client`] over the given `transport`.
    ///
    /// If the Client errors, the error is dropped when `new` is used -- use `with_error_handler`
    /// to handle such an error explicitly.
    pub fn new(transport: T) -> Self where {
        Self::with_error_handler(transport, |_| {})
    }

    /// Construct a new [`Client`] over the given `transport`.
    ///
    /// If the `Client` errors, its error is passed to `on_service_error`.
    pub fn with_error_handler<F>(transport: T, on_service_error: F) -> Self
    where
        F: FnOnce(E) + Send + 'static,
    {
        let (tx, rx) = mediator::new();
        let in_flight = Arc::new(atomic::AtomicUsize::new(0));
        tokio_executor::spawn(
            ClientInner {
                mediator: rx,
                responses: Default::default(),
                waiting: None,
                transport,
                in_flight: in_flight.clone(),
                error: PhantomData::<fn(E)>,
                finish: false,
            }
            .map_err(move |e| on_service_error(e)),
        );
        Client {
            mediator: tx,
            in_flight,
            _error: PhantomData,
        }
    }
}

impl<T, E> Future for ClientInner<T, E>
where
    T: Sink + Stream,
    E: From<Error<T>>,
    E: 'static + Send,
    T::SinkItem: 'static + Send,
    T::Item: 'static + Send,
{
    type Item = ();
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, E> {
        // if Stream had a poll_ready, we could call that here to
        // make sure there's room for at least one more request

        if let Some(ClientRequest { req, span, res }) = self.waiting.take() {
            #[cfg(feature = "tracing")]
            let guard = span.enter();
            #[cfg(feature = "tracing")]
            tracing::event!(Level::TRACE, "retry sending request to Sink");

            if let AsyncSink::NotReady(req) = self
                .transport
                .start_send(req)
                .map_err(Error::from_sink_error)?
            {
                #[cfg(feature = "tracing")]
                {
                    tracing::event!(Level::TRACE, "Sink still full; queueing");
                    drop(guard);
                }

                #[cfg(not(feature = "tracing"))]
                let span = ();
                self.waiting = Some(ClientRequest { req, span, res });
            } else {
                #[cfg(feature = "tracing")]
                {
                    tracing::event!(Level::TRACE, "request sent");
                    drop(guard);
                }

                self.responses.push_back(Pending {
                    tx: res,
                    #[cfg(feature = "tracing")]
                    span,
                });
                self.in_flight.fetch_add(1, atomic::Ordering::AcqRel);
            }
        }

        while self.waiting.is_none() {
            // send more requests if we have them
            match self.mediator.try_recv() {
                Async::Ready(Some(ClientRequest { req, span, res })) => {
                    #[cfg(feature = "tracing")]
                    let guard = span.enter();
                    #[cfg(feature = "tracing")]
                    tracing::event!(Level::TRACE, "request received by worker; sending to Sink");

                    if let AsyncSink::NotReady(req) = self
                        .transport
                        .start_send(req)
                        .map_err(Error::from_sink_error)?
                    {
                        assert!(self.waiting.is_none());
                        #[cfg(feature = "tracing")]
                        {
                            tracing::event!(Level::TRACE, "Sink full; queueing");
                            drop(guard);
                        }
                        #[cfg(not(feature = "tracing"))]
                        let span = ();
                        self.waiting = Some(ClientRequest { req, span, res });
                    } else {
                        #[cfg(feature = "tracing")]
                        {
                            tracing::event!(Level::TRACE, "request sent");
                            drop(guard);
                        }
                        self.responses.push_back(Pending {
                            tx: res,
                            #[cfg(feature = "tracing")]
                            span,
                        });
                        self.in_flight.fetch_add(1, atomic::Ordering::AcqRel);
                    }
                }
                Async::Ready(None) => {
                    self.finish = true;
                    break;
                }
                Async::NotReady => {
                    break;
                }
            }
        }

        if self.in_flight.load(atomic::Ordering::Acquire) != 0 {
            // flush out any stuff we've sent in the past
            // don't return on NotReady since we have to check for responses too
            if self.finish && self.waiting.is_none() {
                // we're closing up shop!
                //
                // note that the check for requests.is_empty() is necessary, because
                // Sink::close() requires that we never call start_send ever again!
                //
                // close() implies poll_complete()
                //
                // FIXME: if close returns Ready, are we allowed to call close again?
                self.transport.close().map_err(Error::from_sink_error)?;
            } else {
                self.transport
                    .poll_complete()
                    .map_err(Error::from_sink_error)?;
            }
        }

        // and start looking for replies.
        //
        // note that we *could* have this just be a loop, but we don't want to poll the stream
        // if we know there's nothing for it to produce.
        while self.in_flight.load(atomic::Ordering::Acquire) != 0 {
            match try_ready!(self.transport.poll().map_err(Error::from_stream_error)) {
                Some(r) => {
                    // ignore send failures
                    // the client may just no longer care about the response
                    let pending = self
                        .responses
                        .pop_front()
                        .expect("got a request with no sender?");
                    event!(pending.span, Level::TRACE, "response arrived; forwarding");

                    let sender = pending.tx;
                    let _ = sender.send(ClientResponse {
                        response: r,
                        #[cfg(feature = "tracing")]
                        span: pending.span,
                    });
                    self.in_flight.fetch_sub(1, atomic::Ordering::AcqRel);
                }
                None => {
                    // the transport terminated while we were waiting for a response!
                    // TODO: it'd be nice if we could return the transport here..
                    return Err(E::from(Error::BrokenTransportRecv(None)));
                }
            }
        }

        if self.finish
            && self.waiting.is_none()
            && self.in_flight.load(atomic::Ordering::Acquire) == 0
        {
            // we're completely done once close() finishes!
            try_ready!(self.transport.close().map_err(Error::from_sink_error));
            return Ok(Async::Ready(()));
        }

        // to get here, we must have no requests in flight and have gotten a NotReady from
        // self.mediator.try_recv or self.transport.start_send. we *could* also have messages
        // waiting to be sent (transport.poll_complete), but if that's the case it must also have
        // returned NotReady. so, at this point, we know that all of our subtasks are either done
        // or have returned NotReady, so the right thing for us to do is return NotReady too!
        Ok(Async::NotReady)
    }
}

impl<T, E> Service<T::SinkItem> for Client<T, E>
where
    T: Sink + Stream,
    E: From<Error<T>>,
    E: 'static + Send,
    T::SinkItem: 'static + Send,
    T::Item: 'static + Send,
{
    type Response = T::Item;
    type Error = E;
    type Future = ClientResponseFut<T, E>;

    fn poll_ready(&mut self) -> Result<Async<()>, E> {
        self.mediator
            .poll_ready()
            .map_err(|_| E::from(Error::ClientDropped))
    }

    fn call(&mut self, req: T::SinkItem) -> Self::Future {
        let (tx, rx) = tokio_sync::oneshot::channel();
        #[cfg(feature = "tracing")]
        let span = tracing::Span::current();
        #[cfg(not(feature = "tracing"))]
        let span = ();
        event!(span, Level::TRACE, "issuing request");
        let req = ClientRequest { req, span, res: tx };
        let fut = match self.mediator.try_send(req) {
            Ok(AsyncSink::Ready) => ClientResponseFutInner::Pending(rx),
            Ok(AsyncSink::NotReady(_)) | Err(_) => {
                ClientResponseFutInner::Failed(Some(E::from(Error::TransportFull)))
            }
        };
        fut.into()
    }
}

impl<T, E> tower_load::Load for Client<T, E>
where
    T: Sink + Stream,
{
    type Metric = usize;

    fn load(&self) -> Self::Metric {
        self.in_flight.load(atomic::Ordering::Acquire)
    }
}

// ===== impl SpawnError =====

impl<T> fmt::Display for SpawnError<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            SpawnError::SpawnFailed => write!(f, "error spawning multiplex client"),
            SpawnError::Inner(ref te) => {
                write!(f, "error making new multiplex transport: {:?}", te)
            }
        }
    }
}

impl<T> error::Error for SpawnError<T>
where
    T: error::Error,
{
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            SpawnError::SpawnFailed => None,
            SpawnError::Inner(ref te) => Some(te),
        }
    }

    fn description(&self) -> &str {
        "error creating new multiplex client"
    }
}
