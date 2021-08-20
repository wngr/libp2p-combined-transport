use std::{cell::RefCell, marker::PhantomData, rc::Rc};

use futures::{
    channel::mpsc,
    future::{self, BoxFuture},
    stream::{self, BoxStream},
    FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use libp2p::{
    core::{
        either::{EitherError, EitherFuture, EitherListenStream, EitherOutput},
        transport::ListenerEvent,
    },
    Transport,
};

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
pub struct CombinedTransport<TBase: Transport + Clone, TOuter> {
    base: TBase,
    outer: TOuter,
    proxy: ProxyTransport<TBase>,
    upgrader: MaybeUpgrade<TBase>,
}

impl<TBase, TOuter> CombinedTransport<TBase, TOuter>
where
    TBase: Transport + Clone,
{
    pub fn new(
        base: TBase,
        outer: fn(ProxyTransport<TBase>) -> TOuter,
        upgrader: MaybeUpgrade<TBase>,
    ) -> Self {
        let proxy = ProxyTransport::<TBase>::new();
        let outer = outer(proxy.clone());
        Self {
            base,
            proxy,
            outer,
            upgrader,
        }
    }
}

type MaybeUpgrade<TBase> = fn(&mut <TBase as Transport>::Output) -> bool;

impl<TBase, TOuter> Transport for CombinedTransport<TBase, TOuter>
where
    TBase: Transport + Clone,
    TBase::Listener: Send + 'static,
    TBase::ListenerUpgrade: Send + 'static,
    TBase::Error: Clone + Send + 'static,
    TBase::Output: 'static,
    TOuter: Transport,
    TOuter::Listener: Send + 'static,
    TOuter::ListenerUpgrade: Send + 'static,
    TOuter::Error: 'static,
    TOuter::Output: 'static,
{
    type Output = EitherOutput<TBase::Output, TOuter::Output>;

    type Error = EitherError<TBase::Error, TOuter::Error>;

    //type Listener = EitherListenStream<TBase::Listener, TOuter::Listener>;
    //type Listener: Stream<Item = Result<ListenerEvent<Self::ListenerUpgrade, Self::Error>, Self::Error>>;
    // TODO remove box
    //type Listener: Stream<Item = Result<ListenerEvent<Self::ListenerUpgrade, Self::Error>, Self::Error>>;
    type Listener = BoxStream<
        'static,
        Result<
            //ListenerEvent<Either<TBase::ListenerUpgrade, TOuter::ListenerUpgrade>, Self::Error>,
            ListenerEvent<Self::ListenerUpgrade, Self::Error>,
            Self::Error,
        >,
    >;
    //    type ListenerUpgrade =
    // BoxFuture<'static, Either<TBase::ListenerUpgrade, TOuter::ListenerUpgrade>>;
    //type ListenerUpgrade = EitherFuture<TBase::ListenerUpgrade, TOuter::ListenerUpgrade>;
    //type ListenerUpgrade: Future<Output = Result<Self::Output, Self::Error>>;
    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
    type Dial = EitherFuture<TBase::Dial, TOuter::Dial>;

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
            .map_err(|e| e.map(EitherError::A))?;
        // 3. Create new mpsc::channel, all events emitted by (2) will be cloned and piped into
        //    this tx, with the exception of the Upgrade event
        let (mut tx, rx) = mpsc::channel(256);
        // 4. Move rx into the proxy
        let x = self.proxy.pending.replace(Some(rx));
        debug_assert!(x.is_none());
        // 5. Call listen_on on `TOuter`, which will call listen_on on proxy. Proxy returns tx from
        //    (4)
        let outer_listener = self.outer.listen_on(addr).expect("FIXME");
        debug_assert!(self.proxy.pending.borrow().is_none());
        // 6. Stream returned by (5) will be joined with the one from (2) and returned from the
        //    function
        let upgrader = self.upgrader;
        let combined_listener = stream::select(
            //            base_listener
            //                .map_ok(move |ev| {
            //                    let cloned = match &ev {
            //                        ListenerEvent::NewAddress(a) => Some(ListenerEvent::NewAddress(a.clone())),
            //                        ListenerEvent::AddressExpired(a) => {
            //                            Some(ListenerEvent::AddressExpired(a.clone()))
            //                        }
            //                        ListenerEvent::Error(e) => Some(ListenerEvent::Error(e.clone())),
            //                        ListenerEvent::Upgrade { .. } => None,
            //                    };
            //                    if let Some(ev) = cloned {
            //                        tx.start_send(ev).unwrap();
            //                    }
            //                    ev.map(|upgrade_fut|
            //                        async move {
            //                        match upgrade_fut.await {
            //Ok(u) => {
            //
            //
            //
            //                                // We could try to upgrade here; if it works, we emit an
            //                                // error for the base transport, and send the whole event over to
            //                                // the outer transport via `tx`. If the upgrade fails, we just
            //                                // continue.
            //                                if upgrader(&mut u) {
            //                                    // yay to outer
            //                                } else {
            //                                    // continue
            //                                }
            //                                Ok(EitherOutput::First(u))
            //},
            //
            //                            Err(e) => Err(EitherError::A(e))
            //}

            //                       }.boxed()
            //)

            //            .map_err(|e| EitherError::A(e))
            //           .boxed()
            //                    })
            //                   .map_err(EitherError::A)
            //})
            //                       {
            //                           upgrade_fut
            //                            .map_ok(EitherOutput::Second)
            //                            .map_err(|e| EitherError::A(e))
            //                            .boxed()
            //                       }
            //                            .map_err(|e| EitherError::A(e))
            //                )
            //                .map_err(EitherError::A)
            //                .boxed(),
            base_listener
                .map_ok(move |mut ev| {
                    let cloned = match &ev {
                        ListenerEvent::NewAddress(a) => Some(ListenerEvent::NewAddress(a.clone())),
                        ListenerEvent::AddressExpired(a) => {
                            Some(ListenerEvent::AddressExpired(a.clone()))
                        }
                        ListenerEvent::Error(e) => Some(ListenerEvent::Error(e.clone())),
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
                                    Ok(mut u) => {
                                        let mut u: TBase::Output = u;
                                        // We could try to upgrade here; if it works, we emit an
                                        // error for the base transport, and send the whole event over to
                                        // the outer transport via `tx`. If the upgrade fails, we just
                                        // continue.
                                        if upgrader(&mut u) {
                                            // yay to outer
                                            tx_c.start_send(ListenerEvent::Upgrade {
                                                // FUCK!
                                                // TBase::ListenerUpgrade is generic
                                                // Maybe this can be TransportProxy::Output?
                                                upgrade: todo!(), // future::ok(u).boxed(),
                                                local_addr: local_addr_c,
                                                remote_addr: remote_addr_c,
                                            })
                                            .expect("FIXME");
                                            panic!("FIXME Create an custom error :-)");
                                        } else {
                                            // continue
                                            Ok(EitherOutput::First(u))
                                        }
                                    }
                                    Err(e) => Err(EitherError::A(e)),
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

                    ev
                        //                    ev.map(|upgrade_fut| {
                        //                        async move {
                        //                            match upgrade_fut.await {
                        //                                Ok(mut u) => {
                        //                                    // We could try to upgrade here; if it works, we emit an
                        //                                    // error for the base transport, and send the whole event over to
                        //                                    // the outer transport via `tx`. If the upgrade fails, we just
                        //                                    // continue.
                        //                                    if upgrader(&mut u) {
                        //                                        // yay to outer
                        //                                    } else {
                        //                                        // continue
                        //                                    }
                        //                                    Ok(EitherOutput::First(u))
                        //                                }
                        //                                Err(e) => Err(EitherError::A(e)),
                        //                            }
                        //                        }
                        //                        .boxed()
                        //                        //                        upgrade_fut
                        //                        //                            .map_ok(EitherOutput::First)
                        //                        //                            .map_err(EitherError::A)
                        //                        //                            .boxed()
                        //                    })
                        .map_err(EitherError::A)
                })
                .map_err(EitherError::A)
                .boxed(),
            outer_listener
                .map_ok(|ev| {
                    ev.map(|upgrade_fut| {
                        upgrade_fut
                            .map_ok(EitherOutput::Second)
                            .map_err(EitherError::B)
                            .boxed()
                    })
                    .map_err(EitherError::B)
                })
                .map_err(EitherError::B)
                .boxed(),
        )
        .boxed();
        // 7. On an upgrade, check the switch, and route it either via outer or directly out
        // TODO!
        Ok(combined_listener)
    }

    fn dial(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Dial, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        todo!()
    }

    fn address_translation(
        &self,
        listen: &libp2p::Multiaddr,
        observed: &libp2p::Multiaddr,
    ) -> Option<libp2p::Multiaddr> {
        todo!()
    }
}

pub struct ProxyTransport<TBase: Transport> {
    _marker: PhantomData<TBase>,
    pending:
        Rc<RefCell<Option<mpsc::Receiver<ListenerEvent<TBase::ListenerUpgrade, TBase::Error>>>>>,
}
impl<TBase> Clone for ProxyTransport<TBase>
where
    TBase: Transport + Clone,
{
    fn clone(&self) -> Self {
        Self {
            _marker: Default::default(),
            pending: self.pending.clone(),
        }
    }
}

impl<TBase> ProxyTransport<TBase>
where
    TBase: Transport + Clone,
{
    fn new() -> Self {
        Self {
            pending: Default::default(),
            _marker: Default::default(),
        }
    }
}

impl<TBase> Transport for ProxyTransport<TBase>
where
    TBase: Transport + Clone,
{
    type Output = TBase::Output;

    type Error = TBase::Error;

    type Listener = TBase::Listener;

    type ListenerUpgrade = TBase::ListenerUpgrade;

    type Dial = TBase::Dial;

    fn listen_on(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Listener, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        todo!()
    }

    fn dial(
        self,
        addr: libp2p::Multiaddr,
    ) -> Result<Self::Dial, libp2p::TransportError<Self::Error>>
    where
        Self: Sized,
    {
        todo!()
    }

    fn address_translation(
        &self,
        listen: &libp2p::Multiaddr,
        observed: &libp2p::Multiaddr,
    ) -> Option<libp2p::Multiaddr> {
        todo!()
    }
}
