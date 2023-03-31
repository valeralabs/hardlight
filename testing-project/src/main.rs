// have to use this as rust doesn't have a stablised feature in nightly yet
// see: https://github.com/rust-lang/rust/issues/91611
use async_trait::async_trait;
use bytecheck::CheckBytes;
use rkyv::{Archive, Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use std::{sync::{Mutex, MutexGuard, Arc}, ops::{Deref, DerefMut}, default};

fn main() {
    println!("Hello, world!");
}

#[async_trait]
trait Counter {
    async fn increment(&self, amount: u32) -> u32;
    async fn decrement(&self, amount: u32) -> u32;
    async fn get(&self) -> u32;
}

#[derive(Clone)]
struct State {
    counter: u32,
}

// currently implementing everything manually to work out what functionality
// the macros will need to provide

// runtime and stuff we can put in the root project and keep those as we move along
// but no macros for time being

// RPC server that implements the Counter trait
struct Handler {
    state: CounterConnectionState,
}

enum Error {
    BadInputBytes,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
struct RpcCall {
    method: Method,
    args: Vec<u8>,
}

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

impl Handler {
    async fn handle_rpc_call(&mut self, input: &[u8]) -> Result<Vec<u8>, Error> {
        let call: RpcCall = match rkyv::from_bytes(input) {
            Ok(call) => call,
            Err(_) => return Err(Error::BadInputBytes),
        };

        match call.method {
            Method::Increment => {
                let args: IncrementArgs = match rkyv::from_bytes(&call.args) {
                    Ok(args) => args,
                    Err(_) => return Err(Error::BadInputBytes),
                };
                let result = self.increment(args.amount).await;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            Method::Decrement => {
                let args: DecrementArgs = match rkyv::from_bytes(&call.args) {
                    Ok(args) => args,
                    Err(_) => return Err(Error::BadInputBytes),
                };
                let result = self.decrement(args.amount).await;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
            Method::Get => {
                let result = self.get().await;
                let result = rkyv::to_bytes::<u32, 1024>(&result).unwrap();
                Ok(result.to_vec())
            }
        }
    }
}

#[async_trait]
impl Counter for Handler {
    async fn increment(&self, amount: u32) -> u32 {
        // lock the state to the current thread
        let mut state = self.state.lock().unwrap();
        state.counter += amount;
        state.counter
    } // state is automatically unlocked here; any changes are sent to the client automagically ✨

    async fn decrement(&self, amount: u32) -> u32 {
        let mut state = self.state.lock().unwrap();
        state.counter -= amount;
        state.counter
    }

    async fn get(&self) -> u32 {
        let state = self.state.lock().unwrap();
        state.counter
    }
}


#[derive(Debug)]
enum ConnectionStateError {
    Poisoned,
}

/// ConnectionState is a wrapper around the user's state that will be the "owner" of a connection's state
struct CounterConnectionState
{
    /// State is locked under an internal mutex so multiple threads can use it safely
    state: Mutex<State>,
    /// The channel is given by the runtime when it creates the connection, allowing us to tell the runtime when the connection's state is modified so it can send the changes to the client automatically
    channel: Arc<Sender<Vec<(String, Vec<u8>)>>>,
}

trait ConnectionState {
    fn new(channel: Sender<Vec<(String, Vec<u8>)>>) -> Self;
    fn lock(&self) -> Result<StateGuard, ConnectionStateError>;
}

impl ConnectionState for CounterConnectionState
{
    fn new(channel: Sender<Vec<(String, Vec<u8>)>>) -> Self {
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
    fn lock(&self) -> Result<StateGuard, ConnectionStateError> {
        match self.state.lock() {
            Ok(state) => Ok(StateGuard {
                starting_state: state.clone(),
                state,
                channel: self.channel.clone(),
            }),
            Err(_) => Err(ConnectionStateError::Poisoned),
        }
    }
}

/// StateGuard is effectively a MutexGuard that sends any changes back to the runtime when it's dropped
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
                rkyv::to_bytes::<u32, 1024>(&self.state.counter).unwrap().to_vec(),
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

// the Deref and DerefMut traits allow us to use the StateGuard as if it were a State (e.g. state.counter instead of state.state.counter)
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
struct Client {}

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
