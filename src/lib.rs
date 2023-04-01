use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rcgen::generate_simple_self_signed;
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use rustls_native_certs::load_native_certs;
use std::{
    io,
    marker::{Send, Sync},
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::SystemTime,
};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{mpsc, oneshot},
};
use tokio_rustls::{
    rustls::{
        client::{ServerCertVerified, ServerCertVerifier},
        Certificate, ClientConfig as TLSClientConfig, PrivateKey, RootCertStore,
        ServerConfig as TLSServerConfig, ServerName,
    },
    server::TlsStream,
    TlsAcceptor,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::{
        client::generate_key,
        server::{Request, Response},
    },
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        http::{HeaderValue, StatusCode},
        Message,
    },
    Connector,
};
use tracing::{debug, info, span, warn, Level};
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
    /// The output bytes for the RPC call were invalid.
    BadOutputBytes,
    /// The connection state was poisoned. This means that the connection
    /// state was dropped while it was locked. This is a fatal error.
    /// If the runtime receives this error, it will close the connection.
    StatePoisoned,
    /// You tried to make an RPC call before your client was connected
    ClientNotConnected,
    /// You've tried to make too many RPC calls at once.
    TooManyCallsInFlight,
}

/// A tokio MPSC channel that is used to send state updates to the runtime.
/// The runtime will then send these updates to the client.
pub type StateUpdateChannel = mpsc::Sender<Vec<(String, Vec<u8>)>>;

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
                debug!("Successfully terminated TLS handshake with {}", peer_addr);
                self.handle_connection(stream, peer_addr);
            }
        }
    }

    fn handle_connection(&self, stream: TlsStream<TcpStream>, peer_addr: SocketAddr) {
        let (state_change_tx, mut state_change_rx) = mpsc::channel(10);
        let handler = (self.factory)(state_change_tx);
        let version: HeaderValue = self.hl_version_string.clone();
        tokio::spawn(async move {
            let span = span!(Level::DEBUG, "connection", peer_addr = %peer_addr);
            let _enter = span.enter();

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
                    debug!(
                        "Received valid handshake, upgrading connection to HardLight ({})",
                        req_version.unwrap().to_str().unwrap()
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

            debug!("Connection fully established");

            // keep track of active RPC calls
            let mut in_flight = [false; u8::MAX as usize];

            let (rpc_tx, mut rpc_rx) = mpsc::channel(u8::MAX as usize);

            let handler = Arc::new(handler);

            debug!("Starting RPC handler loop");
            loop {
                select! {
                    // await new messages from the client
                    Some(msg) = ws_stream.next() => {
                        let msg = msg.unwrap();
                        if msg.is_binary() {
                            let binary = msg.into_data();
                            let msg: ClientMessage = rkyv::from_bytes(&binary).unwrap();

                            match msg {
                                ClientMessage::RPCRequest { id, internal } => {
                                    if in_flight[id as usize] {
                                        warn!("RPC call {id} already in flight. Ignoring.");
                                        continue;
                                    }

                                    debug!("Received RPC call {id} from client. Spawning handler task...");

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

                                    debug!("RPC call {id} handler task spawned.");
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
                        debug!("RPC call {id} finished. Serializing and sending response...");
                        let binary = rkyv::to_bytes::<ServerMessage, 1024>(&msg).unwrap().to_vec();
                        ws_stream.send(Message::Binary(binary)).await.unwrap();
                        debug!("RPC call {id} response sent.");
                    }
                    // await state updates from the application
                    Some(state_changes) = state_change_rx.recv() => {
                        debug!("Received {} state update(s) from application. Serializing and sending...", state_changes.len());
                        let binary = rkyv::to_bytes::<ServerMessage, 1024>(&ServerMessage::StateChange(state_changes)).unwrap().to_vec();
                        ws_stream.send(Message::Binary(binary)).await.unwrap();
                        debug!("State update sent.");
                    }
                }
            }
        });
    }
}

pub struct ClientConfig {
    tls: TLSClientConfig,
    host: String,
}

pub trait State {
    fn apply_changes(&mut self, changes: Vec<(String, Vec<u8>)>) -> HandlerResult<()>;
}

pub struct Client<T>
where
    T: State + Default,
{
    rpc_channel_sender:
        Option<mpsc::Sender<(Vec<u8>, oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>)>>,
    config: ClientConfig,
    state: T,
    hl_version_string: HeaderValue,
}

impl<T> Client<T>
where
    T: State + Default,
{
    /// Creates a new client that doesn't verify the server's certificate.
    pub fn new_self_signed(host: &str) -> Self {
        let tls = TLSClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {}))
            .with_no_client_auth();
        let config = ClientConfig {
            tls,
            host: host.to_string(),
        };
        Self::new_with_config(config)
    }

    /// Create a new client using the system's root certificates.
    pub fn new(host: &str) -> Self {
        let mut root_store = RootCertStore::empty();
        for cert in load_native_certs().unwrap() {
            root_store.add(&Certificate(cert.0)).unwrap();
        }
        let tls = TLSClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let config = ClientConfig {
            tls,
            host: host.to_string(),
        };
        Self::new_with_config(config)
    }

    /// Create a new client using the given configuration.
    pub fn new_with_config(config: ClientConfig) -> Self {
        let version = Version::from_str(HL_VERSION).unwrap();
        Self {
            rpc_channel_sender: None,
            config,
            state: T::default(),
            hl_version_string: format!("hl/{}", version.major).parse().unwrap(),
        }
    }

    pub async fn make_rpc_call(&self, internal: Vec<u8>) -> HandlerResult<Vec<u8>> {
        if let Some(rpc_chan) = self.rpc_channel_sender.clone() {
            let (tx, rx) = oneshot::channel();
            rpc_chan.send((internal, tx)).await.unwrap();
            rx.await.unwrap()
        } else {
            Err(RpcHandlerError::ClientNotConnected)
        }
    }

    pub async fn connect(
        &mut self,
        mut shutdown: mpsc::Receiver<()>,
        channels_tx: oneshot::Sender<(
            mpsc::Sender<(Vec<u8>, oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>)>,
        )>,
    ) {
        let span = span!(Level::DEBUG, "connection", host = self.config.host);
        let _enter = span.enter();

        let connector = Connector::Rustls(Arc::new(self.config.tls.clone()));

        let req = Request::builder()
            .method("GET")
            .header("Host", self.config.host.clone())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key())
            .header("Sec-WebSocket-Protocol", self.hl_version_string.clone())
            .uri(format!("wss://{}/", self.config.host))
            .body(())
            .expect("Failed to build request");

        debug!("Connecting to server...");
        let (mut stream, _) = connect_async_tls_with_config(req, None, Some(connector))
            .await
            .unwrap();
        debug!("Connected to server.");
        debug!("Sending control channels to application...");
        let (rpc_tx, mut rpc_rx) = mpsc::channel(10);
        channels_tx.send((rpc_tx,)).unwrap();
        debug!("Control channels sent.");

        // keep track of active RPC calls
        // we have to do this dumb thing because we can't copy a oneshot::Sender
        let mut active_rpc_calls: [Option<oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>>; 256] = [
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None,
        ];

        debug!("Starting RPC handler loop");
        loop {
            select! {
                // await RPC requests from the application
                Some((internal, completion_tx)) = rpc_rx.recv() => {
                    debug!("Received RPC request from application");
                    // find a free rpc id
                    if let Some(id) = active_rpc_calls.iter().position(|x| x.is_none()) {
                        let span = span!(Level::DEBUG, "rpc", id = id as u8);
                        let _enter = span.enter();
                        debug!("Found free RPC id");

                        let msg = ClientMessage::RPCRequest {
                            id: id as u8,
                            internal
                        };

                        let binary = match rkyv::to_bytes::<ClientMessage, 1024>(&msg) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                warn!("Failed to serialize RPC call. Ignoring. Error: {e}");
                                // we don't care if the receiver has dropped
                                let _ = completion_tx.send(Err(RpcHandlerError::BadInputBytes));
                                return
                            }
                        }.to_vec();

                        debug!("Sending RPC call to server");

                        match stream.send(Message::Binary(binary)).await {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to send RPC call. Ignoring. Error: {e}");
                                // we don't care if the receiver has dropped
                                let _ = completion_tx.send(Err(RpcHandlerError::ClientNotConnected));
                                return
                            }
                        }

                        debug!("RPC call sent to server");

                        active_rpc_calls[id] = Some(completion_tx);
                    } else {
                        warn!("No free RPC id available. Responding with an error.");
                        let _ = completion_tx.send(Err(RpcHandlerError::TooManyCallsInFlight));
                    }
                }
                // await RPC responses from the server
                Some(msg) = stream.next() => {
                    if let Ok(msg) = msg {
                        if let Message::Binary(bytes) = msg {
                            let msg: ServerMessage = match rkyv::from_bytes(&bytes) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    warn!("Received invalid RPC response. Ignoring. Error: {e}");
                                    continue;
                                }
                            };
                            match msg {
                                ServerMessage::RPCResponse { id, output } => {
                                    let span = span!(Level::DEBUG, "rpc", id = id as u8);
                                    let _enter = span.enter();
                                    debug!("Received RPC response from server");
                                    if let Some(completion_tx) = active_rpc_calls[id as usize].take() {
                                        let _ = completion_tx.send(output);
                                    } else {
                                        warn!("Received RPC response for unknown RPC call. Ignoring.");
                                    }
                                }
                                ServerMessage::StateChange(changes) => {
                                    let span = span!(Level::DEBUG, "state_change");
                                    let _enter = span.enter();
                                    debug!("Received {} state change(s) from server", changes.len());
                                    if let Err(e) = self.state.apply_changes(changes) {
                                        warn!("Failed to apply state changes. Error: {:?}", e);
                                    };
                                }
                                ServerMessage::NewEvent { .. } => {
                                    warn!("NewEvent has not been implemented yet. Ignoring.")
                                }
                            }
                        }
                    }
                }
                // await shutdown signal
                _ = shutdown.recv() => {
                    break;
                }
            }
        }

        debug!("RPC handler loop exited.");
    }

    pub fn state(&self) -> &T {
        &self.state
    }
}

struct NoCertificateVerification {}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}
