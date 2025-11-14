# Resolver

The resolver is the coordination layer of the system.  
Relays come and go, clients appear at random, network conditions fluctuate —
the resolver’s job is simply to keep track of who is online right now.

It doesn’t forward data.  
It doesn’t store messages.  
It doesn’t maintain long-term state.  
It just keeps a live view of the network.

Think of it as a directory with a pulse.

---

## What the Resolver Actually Does

A resolver accepts QUIC connections from relays, clients, or other resolvers
and identifies each peer using ALPN. The resolver only cares about relays and
their health. Everything revolves around that.

When a relay connects:

1. The resolver identifies it (`relay/1`)  
2. Receives a `RelayHello`  
3. Responds with `HelloAck`  
4. Waits for periodic heartbeats  
5. Tracks the relay in its live registry  

A relay vanishes → its heartbeat expires → it’s removed.  
Simple, predictable, stateless.

---

## Why Resolvers Exist

Relays are ephemeral. They reboot, restart, move hosts, or vanish for hours.
Clients need a stable discovery point that isn’t one of those relays.

The resolver provides:

- a stable entry point  
- a current list of available relays  
- optional cross-resolver synchronization  
- a neutral place to verify identity and protocol versions  

There’s no “primary” resolver — they are interchangeable.  
A node connects to whichever one responds first.

---

## Communication Model

Everything uses QUIC with ALPN-based role routing:

```rs
enum ProtoRole {
    Resolver,
    Relay,
    Peer,
    Client,
}
```

Each category maps to a dedicated handler.  
There’s no ambiguity about who’s speaking.

Control messages all use single unidirectional streams:

- Relay → Resolver: `RelayHello`, heartbeats  
- Resolver → Relay: `HelloAck`, updates  
- Resolver ↔ Resolver: gossip (optional)  
- Client → Resolver: queries (planned)

Each message is a CBOR payload on its own stream — no multiplexing,
no shared channels, no session semantics. QUIC’s cheap streams make this trivial.

---

## Resolver Philosophy

Resolvers aren’t authoritative. They don’t store anything long-term and don’t
decide topology. They simply observe which relays are reachable and make that
information available.

A resolver should:

- stay online  
- accept incoming peers  
- record what it sees  
- expire stale relays  
- optionally gossip with other resolvers  

If a resolver dies, nothing breaks — relays reconnect to another one.

---

## Lifecycle

A resolver loops through the same simple pattern:

1. Bind QUIC endpoint  
2. Accept connections  
3. Inspect ALPN  
4. Route to appropriate handler  
5. Update internal registry  
6. Clean up expired relays  

There’s no heavy coordination or negotiation.  
Most of the resolver’s work is maintaining timestamps and responding to a
small set of control messages.

---

## High-Level Goals

- Track relays reliably  
- Provide a list of active relays to others  
- Make no assumptions about uptime  
- Handle churn gracefully  
- Stay protocol-agnostic on the data side  

This keeps the resolver small, predictable, and easy to reason about.

---

## Status

The resolver is stable for presence tracking and connection routing.  
Additional capabilities (resolver mesh, querying layer, metrics) can be
added without changing the core behavior.