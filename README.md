# Relay

Relay is a node in a distributed system.  
It maintains a persistent QUIC connection to a resolver and acts as a live
participant in the network. The relay doesn’t own global state; its role is
to announce its presence, stay reachable, and perform whatever responsibilities
the broader system assigns to it.

Think of it as a stateless worker with a heartbeat.

---

## What the Relay Actually Does

The relay boots up, establishes a QUIC endpoint, and tries to connect to one
of the known resolvers. Once connected, it performs a small handshake:

- identify itself  
- prove it isn't a random ghost  
- receive acknowledgement  
- begin a steady heartbeat cycle  

After the handshake, the relay becomes “visible” to the resolver.  
From that point on, the resolver can:

- track whether the relay is alive  
- send control messages  
- inform clients or other nodes that this relay is available  

The relay is built to tolerate churn. If the resolver disappears or the
connection drops, the relay simply retries until it finds another resolver.

---

## Philosophy

A relay is *ephemeral by design*.  
It may die, restart, move hosts, get replaced — none of this should break the
system. The resolver doesn’t depend on any individual relay; it only needs to
know which ones are currently online.

The relay therefore:

- avoids storing long-lived state  
- avoids assuming it will be the “chosen one”  
- avoids coupling with any other relay  
- keeps its logic minimal and reactive  

Everything else in the system should be able to treat relays as interchangeable.

---

## Communication Model

The relay uses unidirectional QUIC streams for all control messages.  
There’s no long-lived “session stream” or multiplexed RPC channel. A message is
a message: one stream, one CBOR payload, done.

This keeps things simple:

- no stream management  
- no backpressure chains  
- no risk of blocking the wrong stream  
- cheap to retry or discard  

QUIC’s design fits this perfectly; streams are disposable and cheap.

---

## Why QUIC?

Because it gives:

- TLS by default  
- true multiplexing  
- no head-of-line blocking  
- clean separation of messages  
- cheap, stateless stream usage  
- simple client/server roles via ALPN  

TCP would force you to build framing.  
WebSockets would force you to fight the protocol.  
QUIC gives exactly what's needed.

---

## Lifecycle

A relay cycle looks like this:

1. Boot  
2. Load keys + config  
3. Open QUIC endpoint  
4. Try resolvers until one accepts  
5. Send `RelayHello`  
6. Receive `HelloAck`  
7. Start heartbeat loop  
8. Accept uni-stream messages from resolver  
9. Repeat steps 7–8 until shutdown or disconnect  
10. On disconnect → back to step 4  

The relay never assumes the world is stable.

---

## High-Level Goals

- Stay online  
- Stay known to the resolver  
