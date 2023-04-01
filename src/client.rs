use std::{sync::Arc, time::SystemTime, str::FromStr};

use futures_util::{SinkExt, StreamExt};
use rustls_native_certs::load_native_certs;
use tokio::{sync::{oneshot, mpsc}, select};
use tokio_rustls::rustls::{ClientConfig as TLSClientConfig, RootCertStore, Certificate, client::{ServerCertVerifier, ServerCertVerified}, ServerName};
use tokio_tungstenite::{tungstenite::{http::{HeaderValue, Request}, handshake::client::generate_key, Message}, Connector, connect_async_tls_with_config};
use tracing::{span, Level, debug, warn};
use version::Version;

use crate::{server::{HandlerResult, HL_VERSION}, wire::{RpcHandlerError, ServerMessage, ClientMessage}};

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
