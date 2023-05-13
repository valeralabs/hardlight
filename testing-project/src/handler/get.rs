use hardlight::*;

use crate::service::Handler;

pub async fn get(connection: &Handler) -> HandlerResult<u32> {
    let state = connection.state.lock();
    Ok(state.counter)
}
