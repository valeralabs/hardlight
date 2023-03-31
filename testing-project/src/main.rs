// have to use this as rust doesn't have a stablised feature in nightly yet
// see: https://github.com/rust-lang/rust/issues/91611
use async_trait::async_trait;
use hardlight::{Handler, HandlerResult, RpcHandlerError, ServerConfig, StateUpdateChannel, Server};
use rkyv::{Archive, Deserialize, Serialize, CheckBytes};
use tokio::sync::mpsc::Sender;

use std::{
    default,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, MutexGuard},
};

#[tokio::main]
async fn main() -> Result<(), std::io::Error>{
    let config = ServerConfig::new_self_signed("localhost");
    let server = Server::new(config, |state_update_channel| {
        Box::new(CounterHandler::new(state_update_channel))
    });

    server.run().await
}

#[async_trait]
trait Counter {
    async fn increment(&self, amount: u32) -> HandlerResult<u32>;
    async fn decrement(&self, amount: u32) -> HandlerResult<u32>;
    // We'll deprecate this at some point as we can just send it using Events
    async fn get(&self) -> HandlerResult<u32>;
}

#[derive(Clone)]
struct State {
    counter: u32,
}

// enum Events {
//     Increment(u32),
//     Decrement(u32),
// }

// currently implementing everything manually to work out what functionality
// the macros will need to provide

// runtime and stuff we can put in the root project and keep those as we move
// along but no macros for time being

// RPC server that implements the Counter trait
struct CounterHandler {
    // the runtime will provide the state when it creates the handler
    state: Arc<CounterConnectionState>,
}

// generated argument structs

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct IncrementArgs {
    amount: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct DecrementArgs {
    amount: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct RpcCall {
    method: Method,
    args: Vec<u8>,
}

#[async_trait]
impl Handler for CounterHandler {
    fn new(state_update_channel: StateUpdateChannel) -> Self {
        Self {
            state: Arc::new(CounterConnectionState::new(state_update_channel)),
        }
    }

    async fn handle_rpc_call(&self, input: &[u8]) -> Result<Vec<u8>, RpcHandlerError> {
        let call: RpcCall = rkyv::from_bytes(input).map_err(|_| RpcHandlerError::BadInputBytes)?;

        match call.method {
            Method::Increment => {
                let args: IncrementArgs =
                    rkyv::from_bytes(&call.args).map_err(|_| RpcHandlerError::BadInputBytes)?;
                let result = self.increment(args.amount).await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            Method::Decrement => {
                let args: DecrementArgs =
                    rkyv::from_bytes(&call.args).map_err(|_| RpcHandlerError::BadInputBytes)?;
                let result = self.decrement(args.amount).await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            Method::Get => {
                let result = self.get().await?;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
        }
    }
}

#[async_trait]
impl Counter for CounterHandler {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        // lock the state to the current thread
        let mut state: StateGuard = self.state.lock()?;
        state.counter += amount;
        Ok(state.counter)
    } // state is automatically unlocked here; any changes are sent to the client
      // automagically ✨

    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        let mut state = self.state.lock()?;
        state.counter -= amount;
        Ok(state.counter)
    }

    async fn get(&self) -> HandlerResult<u32> {
        let state = self.state.lock()?;
        Ok(state.counter)
    }
}

/// ConnectionState is a wrapper around the user's state that will be the
/// "owner" of a connection's state
struct CounterConnectionState {
    /// State is locked under an internal mutex so multiple threads can use it
    /// safely
    state: Mutex<State>,
    /// The channel is given by the runtime when it creates the connection,
    /// allowing us to tell the runtime when the connection's state is modified
    /// so it can send the changes to the client automatically
    channel: Arc<Sender<Vec<(String, Vec<u8>)>>>,
}

impl CounterConnectionState {
    fn new(channel: StateUpdateChannel) -> Self {
        Self {
            // use default values for the state
            state: Mutex::new(State {
                counter: default::Default::default(),
            }),
            channel: Arc::new(channel),
        }
    }

    /// locks the state to the current thread by providing a StateGuard
    /// the StateGuard gets
    fn lock(&self) -> Result<StateGuard, RpcHandlerError> {
        match self.state.lock() {
            Ok(state) => Ok(StateGuard {
                starting_state: state.clone(),
                state,
                channel: self.channel.clone(),
            }),
            Err(_) => Err(RpcHandlerError::StatePoisoned),
        }
    }
}

/// StateGuard is effectively a MutexGuard that sends any changes back to the
/// runtime when it's dropped. We have to generate it in a custom way because
struct StateGuard<'a> {
    /// The StateGuard is given ownership of a lock to the state
    state: MutexGuard<'a, State>,
    /// A copy of the state before we locked it
    /// We use this to compare changes when the StateGuard is dropped
    starting_state: State,
    /// A channel pointer that we can use to send changes to the runtime
    /// which will handle sending them to the client
    channel: Arc<Sender<Vec<(String, Vec<u8>)>>>,
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
            channel.send(changes).await.unwrap();
        });
    }
}

// the Deref and DerefMut traits allow us to use the StateGuard as if it were a
// State (e.g. state.counter instead of state.state.counter)
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

// RPC client that implements the Counter trait
// struct Client {}

// we need to be able to serialise and deserialise the method enum
// so we can match it on the server side
#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
#[repr(u8)]
/// The RPC method to call on the server
enum Method {
    Increment,
    Decrement,
    Get,
}
