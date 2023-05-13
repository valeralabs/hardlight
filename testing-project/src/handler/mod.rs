mod decrement;
mod get;
mod increment;

use crate::service::*;
use hardlight::*;

use self::{decrement::*, get::*, increment::*};

#[rpc_handler]
impl Counter for Handler {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        increment(self, amount).await
    }

    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        decrement(self, amount).await
    }

    async fn get(&self) -> HandlerResult<u32> {
        get(self).await
    }
}
