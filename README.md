# HardLight
A realtime, low-latency encrypted RPC binary protocol using WebSockets.

Named after the fictional [Forerunner technology](https://www.halopedia.org/Hard_light) that "allows light to be transformed into a solid state, capable of bearing weight and performing a variety of tasks".

## Why WebSockets?

WebSockets actually have very little abstraction over a TCP stream. From [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455#section-1.5):

> Conceptually, WebSocket is really just a layer on top of TCP that does the following:
>   -  adds a web origin-based security model for browsers
>   -  adds an addressing and protocol naming mechanism to support multiple services on one port and multiple host names on one IP address
>   -  layers a framing mechanism on top of TCP to get back to the IP packet mechanism that TCP is built on, but without length limits
>   -  includes an additional closing handshake in-band that is designed to work in the presence of proxies and other intermediaries

In effect, we gain the benefits of TLS, wide adoption & firewall support (it runs alongside HTTPS on TCP 443) while having little downsides. This means HardLight is usable in browsers, which was a requirement we had for the framework. In fact, we officially support using HardLight from browsers using the "wasm" feature.

## Install

```
cargo add hardlight
```

## Usage
