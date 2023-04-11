use std::{collections::HashMap, io, net::SocketAddr, str::FromStr, sync::Arc};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rcgen::generate_simple_self_signed;
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{
        broadcast::{self, error::TryRecvError},
        mpsc, oneshot,
    },
};
use tokio_rustls::{
    rustls::{Certificate, PrivateKey, ServerConfig as TLSServerConfig},
    TlsAcceptor,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        http::{HeaderValue, StatusCode},
        Message,
    },
};
use tracing::{debug, info, span, warn, Instrument, Level};
use version::{version, Version};

use crate::{
    wire::{ClientMessage, RpcHandlerError, ServerMessage},
    Topic,
};

/// A tokio MPSC channel that is used to send state updates to the runtime.
/// The runtime will then send these updates to the client.
pub type StateUpdateChannel = mpsc::Sender<Vec<(String, Vec<u8>)>>;

pub type HandlerResult<T> = Result<T, RpcHandlerError>;

/// A [ServerHandler] will be created for each connection to the server.
/// These are user-defined structs that respond to RPC calls
#[async_trait]
pub trait ServerHandler {
    /// Create a new handler using the given state update channel.
    fn new(
        state_update_channel: StateUpdateChannel,
        subscription_tx: mpsc::Sender<Topic>,
    ) -> Self
    where
        Self: Sized;
    /// Handle an RPC call (method + arguments) from the client.
    async fn handle_rpc_call(
        &self,
        input: &[u8],
    ) -> Result<Vec<u8>, RpcHandlerError>;
    // An easy way to get the handler factory.
    // Currently disabled because we can't use impl Trait in traits yet. (https://github.com/rust-lang/rust/issues/91611)
    // fn init() -> impl Fn(StateUpdateChannel) -> Box<dyn Handler + Send +
    // Sync> + Send + Sync + 'static + Copy;
    async fn subscribe(&self, topic: Topic) -> HandlerResult<()>;
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub address: String,
    pub version_major: u32,
    pub tls: TLSServerConfig,
}

impl ServerConfig {
    pub fn new_self_signed(host: &str) -> Self {
        Self::new(host, {
            let cert = generate_simple_self_signed(vec![host.into()]).unwrap();
            TLSServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(
                    vec![Certificate(cert.serialize_der().unwrap())],
                    PrivateKey(cert.serialize_private_key_der()),
                )
                .expect("failed to create TLS config")
        })
    }

    pub fn new(host: &str, tls: TLSServerConfig) -> Self {
        Self {
            address: host.into(),
            version_major: Version::from_str(HL_VERSION).unwrap().major,
            tls,
        }
    }
}

pub const HL_VERSION: &str = version!();

// pub type HandlerFactory = impl Fn(broadcast::Sender<Vec<u8>>, String) ->
// Box<dyn TopicHandler + Send + Sync> + Send
// + Sync
// + 'static
// + Copy;

/// The HardLight server, using tokio & tungstenite.
pub struct Server<T>
where
    T: Fn(
        StateUpdateChannel,
        mpsc::Sender<Topic>,
    ) -> Box<dyn ServerHandler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    /// The server's configuration.
    pub config: ServerConfig,
    /// A closure that creates a new handler for each connection.
    /// The closure is passed a [StateUpdateChannel] that the handler can use
    /// to send state updates to the runtime.
    pub rpc_factory: T,
    pub hl_version_string: HeaderValue,
}

impl<T> Server<T>
where
    T: Fn(
        StateUpdateChannel,
        mpsc::Sender<Topic>,
    ) -> Box<dyn ServerHandler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    pub fn new(config: ServerConfig, factory: T) -> Self {
        Self {
            hl_version_string: format!("hl/{}", config.version_major)
                .parse()
                .unwrap(),
            config,
            rpc_factory: factory,
        }
    }

    pub async fn run(
        &self,
        mut shutdown: oneshot::Receiver<()>,
        control_channels_tx: oneshot::Sender<()>,
    ) -> io::Result<()> {
        info!("Booting HL server v{}...", HL_VERSION);
        let acceptor = TlsAcceptor::from(Arc::new(self.config.tls.clone()));
        let listener = TcpListener::bind(&self.config.address).await?;
        info!("Listening on {} with TLS", self.config.address);

        // oneshot: 1 sender, 1 receiver
        // mpsc: many senders, 1 receiver
        // broadcast: many senders, many receivers
        let (shutdown_tx, _) = broadcast::channel(1);
        control_channels_tx.send(()).unwrap();

        let mut event_channels: HashMap<Topic, broadcast::Sender<Vec<u8>>> =
            HashMap::new();
        let (event_subscription_tx, mut event_subscription_rx) =
            mpsc::channel(10);

        loop {
            select! {
                _ = &mut shutdown => {
                    info!("Shutting down server");
                    if let Err(e) = shutdown_tx.send(()) {
                        warn!("Failed to send shutdown signal: {}", e);
                    };
                    return Ok(());
                }
                Ok((stream, peer_addr)) = listener.accept() => self.handle_connection(stream, acceptor.clone(), peer_addr, shutdown_tx.subscribe(), event_subscription_tx.clone()),
                Some((topic, receiver_sender)) = event_subscription_rx.recv() => {
                    if let Some(sender) = event_channels.get(&topic) {
                        // already a topic handler for this topic, just send the receiver
                        receiver_sender.send(sender.subscribe()).unwrap();
                        continue;
                    } else {
                        // we need to spawn a new topic handler
                        let (sender, receiver) = broadcast::channel(10);
                        event_channels.insert(topic.clone(), sender);
                    }
                }
            }
        }
    }

    fn handle_connection(
        &self,
        stream: TcpStream,
        acceptor: TlsAcceptor,
        peer_addr: SocketAddr,
        mut shutdown: broadcast::Receiver<()>,
        event_subscription_tx: mpsc::Sender<(
            Topic,
            oneshot::Sender<broadcast::Receiver<Vec<u8>>>,
        )>,
    ) {
        let (state_change_tx, mut state_change_rx) = mpsc::channel(10);
        let (topic_subscription_tx, mut topic_subscription_rx) =
            mpsc::channel(10);
        let handler =
            (self.rpc_factory)(state_change_tx, topic_subscription_tx);
        let version: HeaderValue = self.hl_version_string.clone();
        tokio::spawn(async move {
            let connection_span =
                span!(Level::DEBUG, "connection", peer_addr = %peer_addr);

            async move {
                let stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(_) => return,
                };

                debug!("Successfully terminated TLS handshake");

                let callback = |req: &Request, mut response: Response| {
                    // request is only valid if req.headers().get("Sec-WebSocket-Protocol") is
                    // Some(req_version) AND req_version == version
                    let req_version = req.headers().get("Sec-WebSocket-Protocol");
                    if req_version.is_none() || req_version.unwrap() != &version {
                        *response.status_mut() = StatusCode::BAD_REQUEST;
                        warn!("Invalid request from {}, version mismatch (client gave {:?}, server wanted {:?})", peer_addr, req_version, Some(version));
                        return Ok(response);
                    } else {
                        let headers = response.headers_mut();
                        headers.append("Sec-WebSocket-Protocol", version);
                        debug!("Received valid handshake, upgrading connection to HardLight ({})", req_version.unwrap().to_str().unwrap());
                        Ok(response)
                    }
                };

                let mut ws_stream = match accept_hdr_async(stream, callback).await {
                    Ok(ws_stream) => ws_stream,
                    Err(e) => {
                        warn!("Error accepting connection from {}: {}", peer_addr, e);
                        return;
                    }
                };

                debug!("Connection fully established");

                // keep track of active RPC calls
                let mut in_flight = [false; u8::MAX as usize + 1];

                let mut event_subscriptions = vec![];

                let (rpc_tx, mut rpc_rx) = mpsc::channel(u8::MAX as usize + 1);

                let handler = Arc::new(handler);

                debug!("Starting RPC handler loop");
                loop {
                    select! {
                        // await new messages from the client
                        Some(msg) = ws_stream.next() => {
                            let msg = match msg {
                                Ok(msg) => msg,
                                Err(e) => {
                                    warn!("Error receiving message from client: {}", e);
                                    continue;
                                }
                            };
                            if msg.is_binary() {
                                let binary = msg.into_data();
                                let msg: ClientMessage = rkyv::from_bytes(&binary).unwrap();

                                match msg {
                                    ClientMessage::RPCRequest { id, internal } => {
                                        let span = span!(Level::DEBUG, "rpc", id = id);
                                        let _enter = span.enter();

                                        if in_flight[id as usize] {
                                            warn!("RPC call already in flight. Ignoring.");
                                            continue;
                                        }

                                        debug!("Received call from client. Spawning handler task...");

                                        let tx = rpc_tx.clone();
                                        let handler = handler.clone();
                                        in_flight[id as usize] = true;
                                        tokio::spawn(async move {
                                            tx.send(
                                                ServerMessage::RPCResponse {
                                                    id,
                                                    output: handler.handle_rpc_call(&internal).await,
                                                }
                                            ).await
                                        });

                                        debug!("Handler task spawned.");
                                    }
                                }
                            }
                        }
                        // await responses from RPC calls
                        Some(msg) = rpc_rx.recv() => {
                            let id = match msg {
                                ServerMessage::RPCResponse { id, .. } => id,
                                _ => unreachable!(),
                            };
                            let span = span!(Level::DEBUG, "rpc", id = id);
                            let _enter = span.enter();
                            in_flight[id as usize] = false;
                            debug!("RPC call finished. Serializing and sending response...");
                            let binary = rkyv::to_bytes::<ServerMessage, 1024>(&msg).unwrap().to_vec();
                            match ws_stream.send(Message::Binary(binary)).await {
                                Ok(_) => debug!("Response sent."),
                                Err(e) => {
                                    warn!("Error sending response to client: {}", e);
                                    continue
                                }
                            };
                        }
                        // await state updates from the application
                        Some(state_changes) = state_change_rx.recv() => {
                            let span = span!(Level::DEBUG, "state_change");
                            let _enter = span.enter();
                            debug!("Received {} state update(s) from application. Serializing and sending...", state_changes.len());
                            let binary = rkyv::to_bytes::<ServerMessage, 1024>(&ServerMessage::StateChange(state_changes)).unwrap().to_vec();
                            match ws_stream.send(Message::Binary(binary)).await {
                                Ok(_) => debug!("State update sent."),
                                Err(e) => {
                                    warn!("Error sending state update to client: {}", e);
                                    continue
                                }
                            };
                        }
                        Some(topic) = topic_subscription_rx.recv() => {
                            let (tx, rx) = oneshot::channel();
                            event_subscription_tx.send((topic, tx)).await.unwrap();
                            match rx.await {
                                Ok(event_receiver) => event_subscriptions.push(event_receiver),
                                Err(e) => warn!("Error subscribing to event topic: {}", e),
                            };
                        }
                        // await shutdown signal
                        _ = shutdown.recv() => {
                            debug!("Received shutdown signal. Closing connection...");
                            return;
                        }
                    }

                    // await any events from event subscriptions
                    for event_receiver in event_subscriptions.iter_mut() {
                        match event_receiver.try_recv() {
                            Ok(event) => {
                                let binary = rkyv::to_bytes::<ServerMessage, 1024>(&ServerMessage::NewEvent{ event }).unwrap().to_vec();
                                match ws_stream.send(Message::Binary(binary)).await {
                                    Ok(_) => debug!("Event sent."),
                                    Err(e) => {
                                        warn!("Error sending event to client: {}", e);
                                        continue
                                    }
                                };
                            }
                            Err(TryRecvError::Empty) => continue,
                            Err(TryRecvError::Closed) => {
                                warn!("Event subscription closed unexpectedly");
                                continue;
                            }
                            Err(TryRecvError::Lagged(_)) => unreachable!("Event subscription should never lag"),
                        }
                    }
                }
            }
            .instrument(connection_span)
            .await
        });
    }
}
