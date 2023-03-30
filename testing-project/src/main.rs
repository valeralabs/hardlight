// have to use this as rust doesn't have a stablised feature in nightly yet
// see: https://github.com/rust-lang/rust/issues/91611
use async_trait::async_trait;
use bytecheck::CheckBytes;
use rkyv::{Archive, Deserialize, Serialize};
use hardlight::Message;
use tokio::sync::mpsc::Sender;

use std::sync::{Mutex, MutexGuard};

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
    state: ConnectionState,
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
        let mut state = self.state.lock().unwrap().state;
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

// self.state is a ConnectionState<State>, which is a wrapper around a MutexGuard<State>
// self.state.lock() returns a StateGuard<State>, which is a wrapper around a MutexGuard<State>
// we do this so we can do custom logic when the StateGuard is dropped

#[derive(Debug)]
enum ConnectionStateError {
    Poisoned,
}

// A wrapper around a mutex of the state which allows us to send changes to the client
struct ConnectionState
{
    state: Mutex<State>,
}

impl ConnectionState
{
    fn lock(&self) -> Result<StateGuard, ConnectionStateError> {
        match self.state.lock() {
            Ok(state) => Ok(StateGuard {
                state,
                starting_state: state.clone(),
            }),
            Err(e) => Err(ConnectionStateError::Poisoned),
        }
    }
}

struct StateGuard<'a> {
    state: MutexGuard<'a, State>,
    starting_state: State,
    channel: Sender<Vec<(String, Vec<u8>)>>,
}

impl<'a> Drop for StateGuard<'a> {
    fn drop(&mut self) {
        // we compare the starting state to the current state
        // if they're different, we send the changed fields to the client
        // if they're the same, we don't do anything

        let mut changes = Vec::new();

        if self.state.counter != self.starting_state.counter {
            changes.push((
                "counter".to_string(),
                rkyv::to_bytes::<u32, 1024>(&self.state.counter).unwrap().to_vec(),
            ));
        }

        // we need to somehow send this back to the runtime (??)
        // maybe an  
        self.channel.send(changes).await.unwrap();
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
