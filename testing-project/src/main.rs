// have to use this as rust doesn't have a stablised feature in nightly yet
// see: https://github.com/rust-lang/rust/issues/91611
use async_trait::async_trait;
use hardlight::{
    tungstenite, Client, ClientState, HandlerResult, RpcHandlerError,
    RpcResponseSender, Server, ServerConfig, ServerHandler, StateUpdateChannel,
    Topic,
};
use parking_lot::{Mutex, MutexGuard};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::{select, sync::broadcast};
use tracing::{debug, error, info};

use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

#[async_trait]
trait TopicHandler {
    fn new(channel: broadcast::Sender<Vec<u8>>, topic: Topic) -> Self
    where
        Self: Sized;
    async fn handle(&mut self);
}

struct EventMonitor {
    channel: broadcast::Sender<Vec<u8>>,
    topic: Topic,
}

#[async_trait]
impl TopicHandler for EventMonitor {
    fn new(channel: broadcast::Sender<Vec<u8>>, topic: Topic) -> Self {
        Self { channel, topic }
    }

    async fn handle(&mut self) {
        loop {
            let event = Event::Increment(1);
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
            let bytes = rkyv::to_bytes::<Event, 1024>(&event).unwrap();
            self.channel.send(bytes.to_vec()).unwrap();
        }
    }
}

impl EventMonitor {
    /// An easier way to get the channel factory
    fn init() -> impl Fn(
        broadcast::Sender<Vec<u8>>,
        Topic,
    ) -> Box<dyn TopicHandler + Send + Sync>
           + Send
           + Sync
           + 'static
           + Copy {
        |channel, topic| Box::new(EventMonitor::new(channel, topic))
    }
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt::init();

    let config = ServerConfig::new_self_signed("localhost:8080");
    info!("{:?}", config);
    let mut server = CounterServer::new(config);
    // server.add_topic_handler(EventMonitor::init()).await;
    server.start().await.unwrap();

    // wait for the server to start
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let mut client = CounterClient::new_self_signed("localhost:8080");
    client.add_event_handler(EventMonitor::init()).await;
    client.connect().await.unwrap();

    let _ = client.increment(1).await;

    client.disconnect(); // demonstrate that we can disconnect and reconnect
    server.stop();
    server.start().await.unwrap();
    client.connect().await.unwrap(); // note: state is reset as we're using a new connection

    let num_tasks = 1;
    let num_increments_per_task = 1;
    info!("Incrementing counter using {num_tasks} tasks with {num_increments_per_task} increments each");
    let first_value = client.get().await.expect("get failed");
    info!("First value: {}", first_value);

    let counter = Arc::new(client);

    let start = tokio::time::Instant::now();

    let mut tasks = Vec::new();
    for _ in 0..num_tasks {
        let counter = counter.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..num_increments_per_task {
                let _ = counter.increment(1).await;
            }
        }));
    }

    for task in tasks {
        task.await.expect("task failed");
    }

    let final_value = counter.get().await.expect("get failed");

    let elapsed = start.elapsed();
    info!(
        "Ran {} increments using {} tasks in {:?}",
        num_tasks * num_increments_per_task,
        num_tasks,
        elapsed
    );
    info!(
        "(mean time/task = {:?})",
        elapsed / (num_tasks * num_increments_per_task)
    );

    info!("Final value: {}", final_value);

    // make sure server-side mutex is working...
    assert!(final_value == first_value + (num_tasks * num_increments_per_task));

    Ok(())
}

#[async_trait]
trait Counter {
    async fn increment(&self, amount: u32) -> HandlerResult<u32>;
    async fn decrement(&self, amount: u32) -> HandlerResult<u32>;
    // We'll deprecate this at some point as we can just send it using Events
    async fn get(&self) -> HandlerResult<u32>;
}

#[derive(Clone, Default, Debug)]
struct State {
    counter: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
enum Event {
    Increment(u32),
    Decrement(u32),
}

// |event: Event| {
//     match event{
//         Event::Increment(value) => {sdasdhasjd}
//         Event::Decrement(value) => {sdasdhasjd}
//     }
// }

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
enum TopicTypes {
    Account(String),
}

#[async_trait]
impl Counter for Handler {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        // lock the state to the current thread
        let mut state: StateGuard = self.state.lock();
        state.counter += amount;
        // self.subscribe("test".to_string()).await;
        Ok(state.counter)
    } // state is automatically unlocked here; any changes are sent to the client
      // automagically ✨

    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        let mut state = self.state.lock();
        state.counter -= amount;
        Ok(state.counter)
    }

    async fn get(&self) -> HandlerResult<u32> {
        let state = self.state.lock();
        Ok(state.counter)
    }
}

////////////// EVERYTHING BELOW WILL BE AUTOGENERATED BY MACROS!! //////////////

////////////////////////////////// SHARED CODE /////////////////////////////////

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
#[repr(u8)]
/// The RPC method to call on the server
enum RpcCall {
    Increment { amount: u32 },
    Decrement { amount: u32 },
    Get,
}

////////////////////////////////// SERVER CODE /////////////////////////////////

struct CounterServer {
    config: ServerConfig,
    // events: mpsc::Sender<()>
    shutdown: Option<oneshot::Sender<()>>,
    control_channels: Option<()>,
}

impl CounterServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            shutdown: None,
            control_channels: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), std::io::Error> {
        let server = Server::new(self.config.clone(), Handler::init());
        let (error_tx, error_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (control_channels_tx, control_channels_rx) = oneshot::channel();

        tokio::spawn(async move {
            if let Err(e) = server.run(shutdown_rx, control_channels_tx).await {
                error_tx.send(e).unwrap()
            };
        });

        select! {
            e = error_rx => {
                error!("Server error: {:?}", e);
                return Err(e.unwrap());
            }
            control_channels = control_channels_rx => {
                self.control_channels = Some(control_channels.unwrap());
                self.shutdown = Some(shutdown_tx);
                Ok(())
            }
        }
    }
    pub fn stop(&mut self) {
        debug!("Telling server to shutdown");
        match self.shutdown.take() {
            Some(shutdown) => {
                let _ = shutdown.send(());
            }
            None => {}
        }
    }
    pub async fn send(event: Event, topic: String) {}
}

/// RPC server that implements the [Counter] trait. A wrapper around
/// [hardlight::Server]
struct Handler {
    // the runtime will provide the state when it creates the handler
    state: Arc<StateController>,
    subscription_tx: mpsc::Sender<Topic>,
}

impl Handler {
    /// An easier way to get the channel factory
    fn init() -> impl Fn(
        StateUpdateChannel,
        mpsc::Sender<Topic>,
    ) -> Box<dyn ServerHandler + Send + Sync>
           + Send
           + Sync
           + 'static
           + Copy {
        |state_update_channel, subscription_tx| {
            Box::new(Handler::new(state_update_channel, subscription_tx))
        }
    }
}

#[async_trait]
impl ServerHandler for Handler {
    fn new(
        state_update_channel: StateUpdateChannel,
        subscription_tx: mpsc::Sender<Topic>,
    ) -> Self {
        Self {
            state: Arc::new(StateController::new(state_update_channel)),
            subscription_tx,
        }
    }

    async fn handle_rpc_call(
        &self,
        input: &[u8],
    ) -> Result<Vec<u8>, RpcHandlerError> {
        let call: RpcCall = rkyv::from_bytes(input)
            .map_err(|_| RpcHandlerError::BadInputBytes)?;

        match call {
            RpcCall::Increment { amount } => {
                let result = self.increment(amount).await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            RpcCall::Decrement { amount } => {
                let result = self.decrement(amount).await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            RpcCall::Get => {
                let result = self.get().await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
        }
    }

    async fn subscribe(&self, topic: Topic) -> HandlerResult<()> {
        Ok(self
            .subscription_tx
            .send(topic)
            .await
            .map_err(|_| RpcHandlerError::FailedToSubscribeToTopic)?)
    }
}

/// The StateController is a server-side wrapper around the user's state. It
/// manages the state under a [parking_lot::Mutex] and stores a [mpsc::Sender]
/// to send any changes. This is accessable through self.state for RPC
/// functions.
struct StateController {
    /// State is locked under an internal mutex so multiple threads can
    /// use it safely
    state: Mutex<State>,
    /// The channel is given by the runtime when it creates the connection,
    /// allowing us to tell the runtime when the connection's state is modified
    /// so it can send the changes to the client automatically
    channel: Arc<mpsc::Sender<Vec<(String, Vec<u8>)>>>,
}

impl StateController {
    fn new(channel: StateUpdateChannel) -> Self {
        Self {
            // use default values for the state
            state: Mutex::new(Default::default()),
            channel: Arc::new(channel),
        }
    }

    /// Locks the state to the current RPC handler by issuing a StateGuard.
    /// When the StateGuard is dropped, it'll send any changes to the client.
    fn lock(&self) -> StateGuard {
        let state = self.state.lock();
        StateGuard {
            starting_state: state.clone(),
            state,
            channel: self.channel.clone(),
        }
    }
}

/// StateGuard wraps MutexGuard to send any changes back to the runtime when
/// it's dropped.
struct StateGuard<'a> {
    /// The StateGuard is given ownership of a lock to the state
    state: MutexGuard<'a, State>,
    /// A copy of the state before we locked it
    /// We use this to compare changes when the StateGuard is dropped
    starting_state: State,
    /// A channel pointer that we can use to send changes to the runtime
    /// which will handle sending them to the client
    channel: Arc<mpsc::Sender<Vec<(String, Vec<u8>)>>>,
}

impl<'a> Drop for StateGuard<'a> {
    /// Our custom drop implementation will send any changes to the runtime
    fn drop(&mut self) {
        // "diff" the two states to see what changed
        let mut changes = Vec::new();

        if self.state.counter != self.starting_state.counter {
            changes.push((
                "counter".to_string(),
                rkyv::to_bytes::<u32, 1024>(&self.state.counter)
                    .unwrap()
                    .to_vec(),
            ));
        }

        // if there are no changes, don't bother sending anything
        if changes.is_empty() {
            return;
        }

        // send the changes to the runtime
        // we have to spawn a new task because we can't await inside a drop
        let channel = self.channel.clone();
        tokio::spawn(async move {
            // this could fail if the server shuts down before these
            // changes are sent... but we're not too worried about that
            let _ = channel.send(changes).await;
        });
    }
}

// the Deref and DerefMut traits allow us to use the StateGuard as if it were a
// State by derefing to the Mutex's deref impl. This allows us to do
// things like state.counter instead of state.state.counter
impl Deref for StateGuard<'_> {
    type Target = State;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

////////////////////////////////// CLIENT CODE /////////////////////////////////

/// An RPC client wrapping the default runtime client using rustls and
/// tungstenite, implementing the [Counter] trait.
struct CounterClient {
    host: String,
    self_signed: bool,
    shutdown: Option<oneshot::Sender<()>>,
    rpc_tx: Option<mpsc::Sender<(Vec<u8>, RpcResponseSender)>>,
}

impl CounterClient {
    pub fn new_self_signed(host: &str) -> Self {
        Self {
            host: host.to_string(),
            self_signed: true,
            shutdown: None,
            rpc_tx: None,
        }
    }

    #[allow(dead_code)]
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            self_signed: false,
            shutdown: None,
            rpc_tx: None,
        }
    }

    /// Spawns a runtime client in the background to maintain the active
    /// connection
    pub async fn connect(&mut self) -> Result<(), tungstenite::Error> {
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (control_channels_tx, control_channels_rx) = oneshot::channel();
        let (error_tx, error_rx) = oneshot::channel();

        let self_signed = self.self_signed;
        let host = self.host.clone();

        tokio::spawn(async move {
            let mut client: Client<State> = if self_signed {
                Client::new_self_signed(&host)
            } else {
                Client::new(&host)
            };

            if let Err(e) =
                client.connect(shutdown_rx, control_channels_tx).await
            {
                error_tx.send(e).unwrap()
            };
        });

        select! {
            Ok((rpc_tx,)) = control_channels_rx => {
                // at this point, the client will NOT return any errors, so we
                // can safely ignore the error_rx channel
                debug!("Received control channels from client");
                self.shutdown = Some(shutdown);
                self.rpc_tx = Some(rpc_tx);
                Ok(())
            }
            e = error_rx => {
                error!("Error received from client: {:?}", e);
                Err(e.unwrap())
            }
        }
    }

    pub fn disconnect(&mut self) {
        debug!("Telling client to shutdown");
        match self.shutdown.take() {
            Some(shutdown) => {
                let _ = shutdown.send(());
            }
            None => {}
        }
    }

    async fn make_rpc_call(&self, call: RpcCall) -> HandlerResult<Vec<u8>> {
        if let Some(rpc_chan) = self.rpc_tx.clone() {
            let (tx, rx) = oneshot::channel();
            rpc_chan
                .send((
                    rkyv::to_bytes::<RpcCall, 1024>(&call)
                        .map_err(|_| RpcHandlerError::BadInputBytes)?
                        .to_vec(),
                    tx,
                ))
                .await
                .unwrap();
            rx.await.unwrap()
        } else {
            Err(RpcHandlerError::ClientNotConnected)
        }
    }
}

impl Drop for CounterClient {
    fn drop(&mut self) {
        debug!("CounterClient got dropped. Disconnecting.");
        self.disconnect();
    }
}

#[async_trait]
impl Counter for CounterClient {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        match self.make_rpc_call(RpcCall::Increment { amount }).await {
            Ok(c) => rkyv::from_bytes(&c)
                .map_err(|_| RpcHandlerError::BadOutputBytes),
            Err(e) => Err(e),
        }
    }
    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        match self.make_rpc_call(RpcCall::Decrement { amount }).await {
            Ok(c) => rkyv::from_bytes(&c)
                .map_err(|_| RpcHandlerError::BadOutputBytes),
            Err(e) => Err(e),
        }
    }
    // We'll deprecate this at some point as we can just send it using Events
    async fn get(&self) -> HandlerResult<u32> {
        match self.make_rpc_call(RpcCall::Get).await {
            Ok(c) => rkyv::from_bytes(&c)
                .map_err(|_| RpcHandlerError::BadOutputBytes),
            Err(e) => Err(e),
        }
    }
}

impl ClientState for State {
    fn apply_changes(
        &mut self,
        changes: Vec<(String, Vec<u8>)>,
    ) -> HandlerResult<()> {
        for (field, new_value) in changes {
            match field.as_ref() {
                "counter" => {
                    self.counter = rkyv::from_bytes(&new_value)
                        .map_err(|_| RpcHandlerError::BadInputBytes)?
                }
                _ => {}
            }
        }
        Ok(())
    }
}
