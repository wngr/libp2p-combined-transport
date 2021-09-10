use std::panic::panic_any;

use anyhow::Context;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use libp2p::{
    core::{
        either::EitherTransport,
        muxing::StreamMuxerBox,
        transport::{upgrade, Boxed},
    },
    identity, mplex,
    multiaddr::Protocol,
    noise, ping,
    swarm::SwarmBuilder,
    tcp::{tokio::TcpStream, TokioTcpConfig},
    websocket::WsConfig,
    PeerId, Swarm, Transport,
};
use libp2p_combined_transport::CombinedTransport;
use tokio::sync::mpsc;

fn maybe_upgrade(r: TcpStream) -> BoxFuture<'static, Result<TcpStream, TcpStream>> {
    async move {
        let mut buffer = [0; 3];
        if r.0.peek(&mut buffer).await.is_ok() && buffer == *b"GET" {
            println!("It's probably HTTP");
            Ok(r)
        } else {
            println!("It's probably not HTTP");
            Err(r)
        }
    }
    .boxed()
}

fn mk_transport(kind: TransportKind) -> (PeerId, Boxed<(PeerId, StreamMuxerBox)>) {
    let tcp = TokioTcpConfig::new().nodelay(true);
    let base_transport = match kind {
        TransportKind::Tcp => EitherTransport::Left(EitherTransport::Left(tcp)),
        TransportKind::Websocket => {
            EitherTransport::Left(EitherTransport::Right(WsConfig::new(tcp)))
        }
        TransportKind::Combined => EitherTransport::Right(CombinedTransport::new(
            tcp,
            WsConfig::new,
            maybe_upgrade,
            |mut addr| {
                addr.push(Protocol::Ws("/".into()));
                addr
            },
        )),
    };
    let id_keys = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(id_keys.public());
    let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
        .into_authentic(&id_keys)
        .unwrap();

    let transport = base_transport
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
        .multiplex(mplex::MplexConfig::new())
        .boxed();
    (local_peer_id, transport)
}

enum TransportKind {
    Tcp,
    Websocket,
    Combined,
}

fn mk_swarm(kind: TransportKind) -> Swarm<ping::Ping> {
    let (peer_id, transport) = mk_transport(kind);
    let behaviour = ping::Ping::new(ping::PingConfig::new().with_keep_alive(true));
    let b = SwarmBuilder::new(transport, behaviour, peer_id).executor(Box::new(|f| {
        tokio::spawn(f);
    }));
    b.build()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut combined_swarm = mk_swarm(TransportKind::Combined);
    let mut ws_swarm = mk_swarm(TransportKind::Websocket);
    let mut tcp_swarm = mk_swarm(TransportKind::Tcp);

    let (tx, mut rx) = mpsc::channel(2);

    tokio::spawn(async move {
        combined_swarm.listen_on("/ip4/127.0.0.1/tcp/0".parse()?)?;

        while let Some(ev) = combined_swarm.next().await {
            println!("Combined: {:?}", ev);
            match ev {
                libp2p::swarm::SwarmEvent::Behaviour(e) => println!("Combined: {:?}", e),
                libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Combined: Bound to {}", address);
                    tx.send(address).await?;
                }
                libp2p::swarm::SwarmEvent::ConnectionClosed { .. } => panic_any(ev),
                _ => {}
            }
        }

        anyhow::Result::<_, anyhow::Error>::Ok(())
    });

    let mut zero_addr = loop {
        let addr = rx.recv().await.context(":-(")?;
        if matches!(addr.iter().last(), Some(Protocol::Tcp(_))) {
            break addr;
        }
    };

    tcp_swarm.dial_addr(zero_addr.clone())?;

    tokio::spawn(async move {
        while let Some(ev) = tcp_swarm.next().await {
            match ev {
                libp2p::swarm::SwarmEvent::Behaviour(e) => println!("Tcp: {:?}", e),
                libp2p::swarm::SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => println!("Tcp: Connected to {} at {:?}", peer_id, endpoint),
                libp2p::swarm::SwarmEvent::ConnectionClosed { .. } => panic_any(ev),
                _ => {}
            }
        }
    });
    zero_addr.push(Protocol::Ws("/".into()));
    ws_swarm.dial_addr(zero_addr)?;

    while let Some(ev) = ws_swarm.next().await {
        match ev {
            libp2p::swarm::SwarmEvent::Behaviour(e) => println!("Ws: {:?}", e),
            libp2p::swarm::SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => println!("Ws: Connected to {} at {:?}", peer_id, endpoint),
            libp2p::swarm::SwarmEvent::ConnectionClosed { .. } => panic_any(ev),
            _ => {}
        }
    }
    Ok(())
}
