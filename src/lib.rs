use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rcgen::generate_simple_self_signed;
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use rustls::{Certificate, PrivateKey, ServerConfig as TLSConfig};
use std::{
    io,
    marker::{Send, Sync},
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::mpsc::{self, Sender},
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::tungstenite::{
    http::{HeaderValue, StatusCode},
    Message,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
};
use tracing::{info, trace, warn, span, Level};
use version::{version, Version};

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
pub enum ClientMessage {
    /// A message from the client when it calls a method on the server.
    RPCRequest {
        /// A unique counter for each RPC call.
        /// This is used to match responses to requests. It restricts the number
        /// of concurrent operations to 256. Active operations cannot
        /// reuse the same ID, therefore IDs of completed requests can
        /// be reused.
        id: u8,
        /// The internal message serialized with rkyv. This will include the
        /// method name and arguments. The format of this message will slightly
        /// differ depending on the number of methods, and types of arguments.
        /// The macros handle generating the code for this.
        internal: Vec<u8>,
    },
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
pub enum ServerMessage {
    /// A message from the server with the output of a client's method call.
    RPCResponse {
        /// A unique counter for each RPC call.
        /// This is used to match responses to requests. It restricts the number
        /// of concurrent operations to 256. Active operations cannot
        /// reuse the same ID, therefore IDs of completed requests can
        /// be reused.
        id: u8,
        /// The function's output serialized with rkyv. The format of this
        /// message will differ with each application.
        /// The macros handle generating the code for this.
        output: Result<Vec<u8>, RpcHandlerError>,
    },
    /// A message from the server with a new event.
    NewEvent {
        /// The event serialized with rkyv. The format of this will differ
        /// between applications. The macros handle generating the code for
        /// this.
        event: Vec<u8>,
    },
    /// The server updates the connection state.
    StateChange(Vec<(String, Vec<u8>)>),
}

#[derive(Archive, Serialize, Deserialize, Debug)]
#[archive_attr(derive(CheckBytes))]
pub enum RpcHandlerError {
    /// The input bytes for the RPC call were invalid.
    BadInputBytes,
    /// The connection state was poisoned. This means that the connection
    /// state was dropped while it was locked. This is a fatal error.
    /// If the runtime receives this error, it will close the connection.
    StatePoisoned,
}

/// A tokio MPSC channel that is used to send state updates to the runtime.
/// The runtime will then send these updates to the client.
pub type StateUpdateChannel = Sender<Vec<(String, Vec<u8>)>>;

pub type HandlerResult<T> = Result<T, RpcHandlerError>;

/// A [Handler] will be created for each connection to the server.
/// These are user-defined structs that respond to RPC calls
#[async_trait]
pub trait Handler {
    /// Create a new handler using the given state update channel.
    fn new(state_update_channel: StateUpdateChannel) -> Self
    where
        Self: Sized;
    /// Handle an RPC call (method + arguments) from the client.
    async fn handle_rpc_call(&self, input: &[u8]) -> Result<Vec<u8>, RpcHandlerError>;
    // An easy way to get the handler factory.
    // Currently disabled because we can't use impl Trait in traits yet. (https://github.com/rust-lang/rust/issues/91611)
    // fn init() -> impl Fn(StateUpdateChannel) -> Box<dyn Handler + Send + Sync> +
    // Send + Sync + 'static + Copy;
}

#[derive(Debug)]
pub struct ServerConfig {
    pub address: String,
    pub version: Version,
    pub tls: TLSConfig,
}

impl ServerConfig {
    pub fn new_self_signed(host: &str) -> Self {
        Self::new(host, {
            let cert = generate_simple_self_signed(vec![host.into()]).unwrap();
            TLSConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(
                    vec![Certificate(cert.serialize_der().unwrap())],
                    PrivateKey(cert.serialize_private_key_der()),
                )
                .expect("failed to create TLS config")
        })
    }

    pub fn new(host: &str, tls: TLSConfig) -> Self {
        Self {
            address: host.into(),
            version: Version::from_str(HL_VERSION).unwrap(),
            tls,
        }
    }
}

pub const HL_VERSION: &str = version!();

/// The HardLight server, using tokio & tungstenite.
pub struct Server<T>
where
    T: Fn(StateUpdateChannel) -> Box<dyn Handler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    /// The server's configuration.
    pub config: ServerConfig,
    /// A closure that creates a new handler for each connection.
    /// The closure is passed a [StateUpdateChannel] that the handler can use to
    /// send state updates to the runtime.
    pub factory: T,
    pub hl_version_string: HeaderValue,
}

impl<T> Server<T>
where
    T: Fn(StateUpdateChannel) -> Box<dyn Handler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    pub fn new(config: ServerConfig, factory: T) -> Self {
        Self {
            hl_version_string: format!("hl/{}", config.version.major).parse().unwrap(),
            config,
            factory,
        }
    }

    pub async fn run(&self) -> io::Result<()> {
        info!("Booting HL server v{}...", HL_VERSION);
        let acceptor = TlsAcceptor::from(Arc::new(self.config.tls.clone()));
        let listener = TcpListener::bind(&self.config.address).await?;
        info!("Listening on {} with TLS", self.config.address);

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let acceptor = acceptor.clone();

            if let Ok(stream) = acceptor.accept(stream).await {
                trace!("Terminated TLS handshake with {}", peer_addr);
                self.handle_connection(stream, peer_addr);
            }
        }
    }

    fn handle_connection(&self, stream: TlsStream<TcpStream>, peer_addr: SocketAddr) {
        let (state_change_tx, mut state_change_rx) = mpsc::channel(10);
        let handler = (self.factory)(state_change_tx);
        let version: HeaderValue = self.hl_version_string.clone();
        tokio::spawn(async move {
            let span = span!(Level::TRACE, "connection", peer_addr = %peer_addr);
            let _enter = span.enter();
            println!("New connection from {}", peer_addr);

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
                    trace!(
                        "Received valid handshake from {}, upgrading connection",
                        peer_addr
                    );
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

            // keep track of active RPC calls
            let mut in_flight = [false; u8::MAX as usize];

            let (rpc_tx, mut rpc_rx) = mpsc::channel(u8::MAX as usize);

            let handler = Arc::new(handler);

            loop {
                select! {
                    // await new messages from the client
                    Some(msg) = ws_stream.next() => {
                        let msg = msg.unwrap();
                        if msg.is_binary() {
                            println!("Server on message: {:?}", &msg);
                            let binary = msg.into_data();
                            let msg: ClientMessage = rkyv::from_bytes(&binary).unwrap();

                            match msg {
                                ClientMessage::RPCRequest { id, internal } => {
                                    if in_flight[id as usize] {
                                        warn!("RPC call {id} already in flight. Ignoring.");
                                        continue;
                                    }

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
                        in_flight[id as usize] = false;
                        trace!("RPC call {id} finished. Serializing and sending response...");
                        let binary = rkyv::to_bytes::<ServerMessage, 1024>(&msg).unwrap().to_vec();
                        ws_stream.send(Message::Binary(binary)).await.unwrap();
                        trace!("RPC call {id} response sent.");
                    }
                    // await state updates from the runtime
                    Some(state_changes) = state_change_rx.recv() => {
                        let binary = rkyv::to_bytes::<ServerMessage, 1024>(&ServerMessage::StateChange(state_changes)).unwrap().to_vec();
                        ws_stream.send(Message::Binary(binary)).await.unwrap();
                    }
                }
            }
        });
    }
}
