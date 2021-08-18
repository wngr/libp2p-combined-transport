use std::marker::PhantomData;

use libp2p::{
    core::{either::EitherOutput, transport::ListenerEvent},
    futures::{future, stream, Future, FutureExt, TryFutureExt, TryStreamExt},
    Multiaddr, Transport, TransportError,
};

pub struct MaybeUpgrade<TInner, TUpgrade> {
    inner: TInner,
    _marker: PhantomData<TUpgrade>,
}

impl<TInner, TUpgrade> MaybeUpgrade<TInner, TUpgrade> {
    pub fn new(inner: TInner) -> Self {
        Self {
            inner,
            _marker: Default::default(),
        }
    }
}

pub trait UpgradeMaybe<TInner, TUpgrade>
where
    TInner: Transport,
    TUpgrade: Transport,
{
    type UpgradeFuture: Future<Output = Result<TUpgrade::Output, TInner::Output>> + Send;
    fn try_upgrade(_other: TInner::Output) -> Self::UpgradeFuture;
}

#[allow(clippy::type_complexity)]
impl<TInner, TUpgrade> Transport for MaybeUpgrade<TInner, TUpgrade>
where
    TInner: Transport,
    TUpgrade: UpgradeMaybe<TInner, TUpgrade> + Transport,
{
    type Output = EitherOutput<TInner::Output, TUpgrade::Output>;

    type Error = TInner::Error;

    type Listener = stream::MapOk<
        TInner::Listener,
        fn(
            ListenerEvent<TInner::ListenerUpgrade, Self::Error>,
        ) -> ListenerEvent<Self::ListenerUpgrade, Self::Error>,
    >;

    type ListenerUpgrade = future::AndThen<
        TInner::ListenerUpgrade,
        future::Then<
            TUpgrade::UpgradeFuture,
            future::Ready<Result<Self::Output, Self::Error>>,
            fn(
                Result<TUpgrade::Output, TInner::Output>,
            ) -> future::Ready<Result<Self::Output, Self::Error>>,
        >,
        fn(
            TInner::Output,
        ) -> future::Then<
            TUpgrade::UpgradeFuture,
            future::Ready<Result<Self::Output, Self::Error>>,
            fn(
                Result<TUpgrade::Output, TInner::Output>,
            ) -> future::Ready<Result<Self::Output, Self::Error>>,
        >,
    >;

    type Dial = future::MapOk<TInner::Dial, fn(TInner::Output) -> Self::Output>;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        let listener: Self::Listener =
            self.inner
                .listen_on(addr)?
                .map_ok::<_, fn(_) -> _>(|event| {
                    event.map(|upgrade_fut| {
                        upgrade_fut.and_then::<_, fn(_) -> _>(|inner| {
                            TUpgrade::try_upgrade(inner).then::<_, fn(_) -> _>(|res| {
                                match res {
                                    Err(inner) => future::ok(EitherOutput::First(inner)),
                                    Ok(upgraded) => future::ok(EitherOutput::Second(upgraded)),
                                }
                            })
                        })
                    })
                });
        Ok(listener)
    }

    fn dial(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        // TODO: Try dialing with TUpgrade?
        let dial = self
            .inner
            .dial(addr)?
            .map_ok::<_, fn(_) -> _>(EitherOutput::First);
        Ok(dial)
    }

    fn address_translation(&self, listen: &Multiaddr, observed: &Multiaddr) -> Option<Multiaddr> {
        self.inner.address_translation(listen, observed)
    }
}
