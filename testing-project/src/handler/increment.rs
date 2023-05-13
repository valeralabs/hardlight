use hardlight::*;

use crate::service::Handler;

pub async fn increment(
    connection: &Handler,
    amount: u32,
) -> HandlerResult<u32> {
    // lock the state to the current thread
    let mut state = connection.state.lock();
    state.counter += amount;
    Ok(state.counter)
}
