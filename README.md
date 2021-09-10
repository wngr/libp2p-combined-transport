# libp2p-combined-transport

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/wngr/libp2p-combined-transport)
[![Cargo](https://img.shields.io/crates/v/libp2p-combined-transport.svg)](https://crates.io/crates/libp2p-combined-transport)
[![Documentation](https://docs.rs/libp2p-combined-transport/badge.svg)](https://docs.rs/libp2p-combined-transport)

Libp2p Transport combining two other transports. One of which is the
base transport (like TCP), and another one is a higher-level transport
(like WebSocket). Similar to [`OrTransport`], this tries to dial first
with the outer connection, and if that fails, with the base one. The
main difference is that incoming connections can be accepted on either
one of them. For this to work, a switch must be provided when handling
incoming connections. For example for TCP, this can be achieved with
the [`peek`] method on the underlying [`TcpStream`].
[`ListenerEvent`]s from the base transport are cloned and routed to
the outer transport via the [`ProxyTransport`], with the exception of
upgrades.

[`peek`]: https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.peek


For a usage example, have a look at the [TCP-Websocket example](https://github.com/wngr/libp2p-combined-transport/tree/master/examples/tcp-websocket.rs).
