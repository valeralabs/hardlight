# HardLight

A realtime, low-latency encrypted RPC binary protocol using WebSockets.

HardLight has two data models:

- RPC: a client connects to a server, and can call functions on the server
- Subscriptions: a client connects to a server, and can subscribe to events from the server

An example: multiple clients subscribe to a "chat" event using Subscriptions. Clients use an RPC function `send_message` to send a message, which will then persisted by the server and sent to all clients subscribed to the "chat" event.

Named after the fictional [Forerunner technology](https://www.halopedia.org/Hard_light) that "allows light to be transformed into a solid state, capable of bearing weight and performing a variety of tasks".

## Why WebSockets?

WebSockets actually have very little abstraction over a TCP stream. From [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455#section-1.5):

> Conceptually, WebSocket is really just a layer on top of TCP that does the following:
>
> - adds a web origin-based security model for browsers
> - adds an addressing and protocol naming mechanism to support multiple services on one port and multiple host names on one IP address
> - layers a framing mechanism on top of TCP to get back to the IP packet mechanism that TCP is built on, but without length limits
> - includes an additional closing handshake in-band that is designed to work in the presence of proxies and other intermediaries

In effect, we gain the benefits of TLS, wide adoption & firewall support (it runs alongside HTTPS on TCP 443) while having little downsides. This means HardLight is usable in browsers, which was a requirement we had for the framework. In fact, we officially support using HardLight from browsers using the "wasm" feature.

At Valera, we use HardLight to connect clients to our servers, and for connecting some services to one another.

## Install

```shell
cargo add hardlight
```

## Usage

HardLight is designed to be simple, secure and fast. We take advantage of Rust's trait system to allow you to define your own RPC methods, and then use the `#[derive(RPC)]` macro to generate the necessary code to make it work.

Here's a very simple example of a counter service:

```rust
use hardlight::RPC;

#[derive(RPC)]
trait Counter {
    async fn increment(amount: u32) -> u32;
    async fn decrement(amount: u32) -> u32;
    async fn get() -> u32;
}
```

This trait is shared between clients and servers. Any inputs or outputs have to support rkyv's `Serialize`, `Deserialize`, `Archive` and `CheckBytes` traits. This means you can use any primitive type, or any type that implements these traits.

The `#[derive(RPC)]` macro will generate:

- a `Client` struct
- a `Server` struct
- an enum, `Method`, of all the RPC method identifiers (e.g. `Increment`, `Decrement`, `Get`)
- input structs for each RPC method (e.g. `struct IncrementInput { amount: u32 }`)
- output types for each RPC method (e.g. `type IncrementOutput = u32`)

Both structs will expose methods for holding connection state in a hashmap. You can store anything serializable in the hashmap, and it will be available to all RPC methods. This is useful for storing authentication tokens, or other state that you want to be available to all RPC methods.

You would then `impl Counter for CounterServer` to add your functionality. For example:

```rust
use hardlight::HardLight;

static mut COUNTER: u32 = 0;

impl Counter for CounterServer {
    async fn increment(amount: u32) -> u32 {
        unsafe {
            COUNTER += amount;
            COUNTER
        }
    }

    async fn decrement(amount: u32) -> u32 {
        unsafe {
            COUNTER -= amount;
            COUNTER
        }
    }

    async fn get() -> u32 {
        unsafe { COUNTER }
    }
}
```

## Features

- Feature sets: depending on the context, certain endpoints can be disabled or enabled
  - Example: An unauthenticated client only has access to RPC methods to authenticate, and reconnects with authentication with the full set of RPC methods
