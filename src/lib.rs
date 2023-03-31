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
    sync::mpsc::{self, Sender},
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
};
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
        output: Vec<u8>,
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

#[derive(Debug)]
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
    fn new(state_update_channel: StateUpdateChannel) -> Self
    where
        Self: Sized;
    async fn handle_rpc_call(&mut self, input: &[u8]) -> Result<Vec<u8>, RpcHandlerError>;
}

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

pub struct Server<T>
where
    T: Fn(StateUpdateChannel) -> Box<dyn Handler> + Send + Sync + 'static + Copy,
{
    pub config: ServerConfig,
    pub handler: T,
}

impl<T> Server<T>
where
    T: Fn(StateUpdateChannel) -> Box<dyn Handler> + Send + Sync + 'static + Copy,
{
    pub fn new(config: ServerConfig, handler: T) -> Self {
        Self { config, handler }
    }

    pub async fn run(&self) -> io::Result<()> {
        let acceptor = TlsAcceptor::from(Arc::new(self.config.tls.clone()));
        let listener = TcpListener::bind(&self.config.address).await?;

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let acceptor = acceptor.clone();

            if let Ok(stream) = acceptor.accept(stream).await {
                self.handle_connection(stream, peer_addr);
            }
        }
    }

    fn handle_connection(&self, stream: TlsStream<TcpStream>, peer_addr: SocketAddr) {
        let (tx, mut rx) = mpsc::channel(10);
        let handler = (self.handler)(tx);
        tokio::spawn(async move {
            println!("New connection from {}", peer_addr);

            let callback = |req: &Request, mut response: Response| {
                println!("Received a new ws handshake");
                println!("The request's path is: {}", req.uri().path());
                println!("The request's headers are:");
                for (ref header, _value) in req.headers() {
                    println!("* {}: {:?}", header, _value);
                }

                let headers = response.headers_mut();
                headers.append(
                    "Sec-WebSocket-Protocol",
                    format!("hl/{}", self.config.version.major).parse().unwrap(),
                );

                Ok(response)
            };

            let mut ws_stream = accept_hdr_async(stream, callback)
                .await
                .expect("Error during the websocket handshake occurred");

            while let Some(msg) = ws_stream.next().await {
                let msg = msg.unwrap();
                if msg.is_binary() {
                    println!("Server on message: {:?}", &msg);
                    let binary = msg.into_data();
                    let msg: ClientMessage = rkyv::from_bytes(&binary).unwrap();

                    match msg {
                        ClientMessage::RPCRequest { id, internal } => {
                            handler.handle_rpc_call(&internal).await.unwrap();
                        }
                    }
                    

                    ws_stream.send(msg).await.unwrap();
                }
            }
        });
    }
}
