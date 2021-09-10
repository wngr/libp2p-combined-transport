#![allow(clippy::type_complexity)]
use std::{fmt, marker::PhantomData, sync::Arc};

use futures::{
    channel::mpsc,
    future::{self, BoxFuture},
    stream::{self, BoxStream},
    FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use libp2p::{
    core::{either::EitherOutput, transport::ListenerEvent},
    Multiaddr, Transport, TransportError,
};
use parking_lot::Mutex;

/// Transport combining two transports. One of which is the base transport (like TCP), and another
/// one is a higher-level transport (like WebSocket). Similar to [`OrTransport`], this tries to
/// dial first with the outer connection, and if that fails, with the base one. The main difference
/// is that incoming connections can be accepted on either one of them. For this to work, a
/// switch must be provided when handling incoming connections. For TCP, this can be achieved with
/// the [`peek`] method on the underlying [`TcpStream`].
/// [`ListenerEvent`]s from the base transport are cloned and routed to the outer transport via the
/// [`ProxyTransport`], with the exception of upgrades.
///
/// [`peek`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.peek
///
/// For a usage example, have a look at the [TCP-Websocket example](https://github.com/wngr/libp2p-combined-transport/tree/master/examples/tcp-websocket.rs).
pub struct CombinedTransport<TBase, TOuter>
where
    TBase: Transport + Clone,
    TBase::Error: Send + 'static,
    TBase::Output: 'static,
{
    /// The base transport
    base: TBase,
    /// The outer transport, wrapping the base transport
    outer: TOuter,
    /// Function pointer to construct the outer transport, given the base transport
    construct_outer: fn(ProxyTransport<TBase>) -> TOuter,
    proxy: ProxyTransport<TBase>,
    /// Function pointer to try upgrading the base transport to the outer transport
    try_upgrade: MaybeUpgrade<TBase>,
    map_base_addr_to_outer: fn(Multiaddr) -> Multiaddr,
}

impl<TBase, TOuter> CombinedTransport<TBase, TOuter>
where
    TBase: Transport + Clone,
    TBase::Error: Send + 'static,
    TBase::Output: 'static,
{
    /// Construct a new combined transport, given a base transport, a function to construct the
    /// outer transport given the base transport, a function to try the upgrade to the outer
    /// transport given incoming base connections, and a function to map base addresses to outer
    /// addresses (if necessary).
    pub fn new(
        base: TBase,
        construct_outer: fn(ProxyTransport<TBase>) -> TOuter,
        try_upgrade: MaybeUpgrade<TBase>,
        map_base_addr_to_outer: fn(Multiaddr) -> Multiaddr,
    ) -> Self {
        let proxy = ProxyTransport::<TBase>::new(base.clone());
        let mut proxy_clone = proxy.clone();
        proxy_clone.pending = proxy.pending.clone();
        let outer = construct_outer(proxy_clone);
        Self {
            base,
            proxy,
            outer,
            construct_outer,
            try_upgrade,
            map_base_addr_to_outer,
        }
    }
}
impl<TBase, TOuter> Clone for CombinedTransport<TBase, TOuter>
where
    TBase: Transport + Clone,
    TBase::Error: Send + 'static,
    TBase::Output: 'static,
{
    fn clone(&self) -> Self {
        Self::new(
            self.base.clone(),
            self.construct_outer,
            self.try_upgrade,
            self.map_base_addr_to_outer,
        )
    }
}

type MaybeUpgrade<TBase> =
    fn(
        <TBase as Transport>::Output,
    )
        -> BoxFuture<'static, Result<<TBase as Transport>::Output, <TBase as Transport>::Output>>;

#[derive(Debug, Copy, Clone)]
pub enum CombinedError<Base, Outer> {
    UpgradedToOuterTransport,
    Base(Base),
    Outer(Outer),
}
impl<A, B> fmt::Display for CombinedError<A, B>
where
    A: fmt::Display,
    B: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CombinedError::Base(a) => a.fmt(f),
            CombinedError::Outer(b) => b.fmt(f),
            CombinedError::UpgradedToOuterTransport => write!(f, "Upgraded to outer transport"),
        }
    }
}

impl<A, B> std::error::Error for CombinedError<A, B>
where
    A: std::error::Error,
    B: std::error::Error,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CombinedError::Base(a) => a.source(),
            CombinedError::Outer(b) => b.source(),
            CombinedError::UpgradedToOuterTransport => None,
        }
    }
}

impl<TBase, TOuter> Transport for CombinedTransport<TBase, TOuter>
where
    TBase: Transport + Clone,
    TBase::Listener: Send + 'static,
    TBase::ListenerUpgrade: Send + 'static,
    TBase::Error: Send + 'static,
    TBase::Output: Send + 'static,
    TBase::Dial: Send + 'static,
    TOuter: Transport,
    TOuter::Listener: Send + 'static,
    TOuter::ListenerUpgrade: Send + 'static,
    TOuter::Error: 'static,
    TOuter::Output: 'static,
    TOuter::Dial: Send + 'static,
{
    type Output = EitherOutput<TBase::Output, TOuter::Output>;

    type Error = CombinedError<TBase::Error, TOuter::Error>;

    type Listener =
        BoxStream<'static, Result<ListenerEvent<Self::ListenerUpgrade, Self::Error>, Self::Error>>;
    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Listener, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        // 1. User calls `listen_on`
        // 2. Base transport `listen_on` -> returns `TBase::Listener`
        let base_listener = self
            .base
            .listen_on(addr.clone())
            .map_err(|e| e.map(CombinedError::Base))?;
        // 3. Create new mpsc::channel, all events emitted by (2) will be cloned and piped into
        //    this tx, with the exception of the Upgrade event
        let (mut tx, rx) = mpsc::channel(256);
        // 4. Move rx into the proxy
        let x = self.proxy.pending.lock().replace(rx);
        debug_assert!(x.is_none());
        // 5. Call listen_on on `TOuter`, which will call listen_on on proxy. Proxy returns tx from
        //    (4)
        let outer_listener = self
            .outer
            .listen_on((self.map_base_addr_to_outer)(addr))
            .map_err(|e| e.map(CombinedError::Outer))?;
        debug_assert!(self.proxy.pending.lock().is_none());
        // 6. Stream returned by (5) will be joined with the one from (2) and returned from the
        //    function
        let upgrader = self.try_upgrade;
        let combined_listener = stream::select(
            base_listener
                .map_ok(move |ev| {
                    let cloned = match &ev {
                        ListenerEvent::NewAddress(a) => Some(ListenerEvent::NewAddress(a.clone())),
                        ListenerEvent::AddressExpired(a) => {
                            Some(ListenerEvent::AddressExpired(a.clone()))
                        }
                        ListenerEvent::Error(_) => None, // Error is only propagated once, namely for the base transport
                        ListenerEvent::Upgrade { .. } => None,
                    };
                    if let Some(ev) = cloned {
                        tx.start_send(ev).unwrap();
                    }
                    let ev = match ev {
                        ListenerEvent::Upgrade {
                            upgrade,
                            local_addr,
                            remote_addr,
                        } => {
                            let local_addr_c = local_addr.clone();
                            let remote_addr_c = remote_addr.clone();
                            let mut tx_c = tx.clone();
                            let upgrade = async move {
                                match upgrade.await {
                                    Ok(u) => {
                                        // We could try to upgrade here; if it works, we emit an
                                        // error for the base transport, and send the whole event over to
                                        // the outer transport via `tx`. If the upgrade fails, we just
                                        // continue.
                                        match upgrader(u).await {
                                            Ok(u) => {
                                                // yay to outer
                                                tx_c.start_send(ListenerEvent::Upgrade {
                                                    // FUCK!
                                                    // TBase::ListenerUpgrade is generic
                                                    // Maybe this can be TransportProxy::Output?
                                                    // ok, so the type of `tx` needs to be modified to
                                                    // accomodate the types of ProxyTransport
                                                    upgrade: future::ok(u).boxed(),
                                                    local_addr: local_addr_c,
                                                    remote_addr: remote_addr_c,
                                                })
                                                .expect("Out of sync with proxy");
                                                Err(CombinedError::UpgradedToOuterTransport)
                                            }
                                            Err(u) => {
                                                // continue
                                                Ok(EitherOutput::First(u))
                                            }
                                        }
                                    }
                                    Err(e) => Err(CombinedError::Base(e)),
                                }
                            }
                            .boxed();

                            ListenerEvent::Upgrade {
                                local_addr,
                                remote_addr,
                                upgrade,
                            }
                        }
                        ListenerEvent::NewAddress(a) => ListenerEvent::NewAddress(a),
                        ListenerEvent::AddressExpired(a) => ListenerEvent::AddressExpired(a),
                        ListenerEvent::Error(e) => ListenerEvent::Error(e),
                    };

                    ev.map_err(CombinedError::Base)
                })
                .map_err(CombinedError::Base)
                .boxed(),
            outer_listener
                .map_ok(|ev| {
                    ev.map(|upgrade_fut| {
                        upgrade_fut
                            .map_ok(EitherOutput::Second)
                            .map_err(CombinedError::Outer)
                            .boxed()
                    })
                    .map_err(CombinedError::Outer)
                })
                .map_err(CombinedError::Outer)
                .boxed(),
        )
        .boxed();
        // 7. On an upgrade, check the switch, and route it either via outer or directly out
        Ok(combined_listener)
    }

    fn dial(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Dial, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        let addr = match self.outer.dial(addr) {
            Ok(connec) => {
                return Ok(connec
                    .map_ok(EitherOutput::Second)
                    .map_err(CombinedError::Outer)
                    .boxed())
            }
            Err(TransportError::MultiaddrNotSupported(addr)) => addr,
            Err(TransportError::Other(err)) => {
                return Err(TransportError::Other(CombinedError::Outer(err)))
            }
        };

        let addr = match self.base.dial(addr) {
            Ok(connec) => {
                return Ok(connec
                    .map_ok(EitherOutput::First)
                    .map_err(CombinedError::Base)
                    .boxed())
            }
            Err(TransportError::MultiaddrNotSupported(addr)) => addr,
            Err(TransportError::Other(err)) => {
                return Err(TransportError::Other(CombinedError::Base(err)))
            }
        };

        Err(TransportError::MultiaddrNotSupported(addr))
    }

    fn address_translation(
        &self,
        listen: &libp2p::Multiaddr,
        observed: &libp2p::Multiaddr,
    ) -> Option<libp2p::Multiaddr> {
        // Outer probably will call proxy, which will proxy to base
        self.outer
            .address_translation(listen, observed)
            .or_else(|| self.base.address_translation(listen, observed))
    }
}

pub struct ProxyTransport<TBase>
where
    Self: Transport,
{
    _marker: PhantomData<TBase>,
    // 1-1 relation between [`CombinedTransport`] and [`ProxyTransport`]
    pub(crate) pending: Arc<
        Mutex<
            Option<
                mpsc::Receiver<
                    ListenerEvent<<Self as Transport>::ListenerUpgrade, <Self as Transport>::Error>,
                >,
            >,
        >,
    >,
    // Clone of TBase for dialing
    base: TBase,
}

// TODO: simplify all those trait bounds
impl<TBase> Clone for ProxyTransport<TBase>
where
    TBase: Transport + Clone,
    TBase::Output: 'static,
    TBase::Error: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            _marker: Default::default(),
            pending: Default::default(),
            base: self.base.clone(),
        }
    }
}

impl<TBase> ProxyTransport<TBase>
where
    TBase: Transport + Clone,
    TBase::Output: 'static,
    TBase::Error: Send + 'static,
{
    fn new(base: TBase) -> Self {
        Self {
            pending: Default::default(),
            _marker: Default::default(),
            base,
        }
    }
}

impl<TBase> Transport for ProxyTransport<TBase>
where
    TBase: Transport + Clone,
    TBase::Output: 'static,
    TBase::Error: Send + 'static,
{
    type Output = TBase::Output;

    type Error = TBase::Error;

    type Listener =
        BoxStream<'static, Result<ListenerEvent<Self::ListenerUpgrade, Self::Error>, Self::Error>>;

    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    type Dial = TBase::Dial;

    fn listen_on(
        self,
        _addr: libp2p::Multiaddr,
    ) -> Result<Self::Listener, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        let listener = self
            .pending
            .lock()
            .take()
            .expect("Only called after successful base listen");
        Ok(listener.map(Ok).boxed())
    }

    fn dial(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Dial, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        self.base.dial(addr)
    }

    fn address_translation(
        &self,
        listen: &libp2p::Multiaddr,
        observed: &libp2p::Multiaddr,
    ) -> Option<libp2p::Multiaddr> {
        self.base.address_translation(listen, observed)
    }
}
