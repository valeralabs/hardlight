pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}

pub enum Message {
    /// A message from the client when it calls a method on the server.
    RPCRequest {
        /// A unique counter for each RPC call.
        /// This is used to match responses to requests. It restricts the number of
        /// concurrent operations to 256. Active operations cannot reuse the same ID,
        /// therefore IDs of completed requests can be reused.
        id: u8,
        /// The internal message serialized with rkyv. This will include the 
        /// method name and arguments. The format of this message will slightly 
        /// differ depending on the number of methods, and types of arguments.
        /// The macros handle generating the code for this.
        internal: Vec<u8>
    }, 
    /// A message from the server with the output of a client's method call.
    RPCResponse {
        /// A unique counter for each RPC call.
        /// This is used to match responses to requests. It restricts the number of
        /// concurrent operations to 256. Active operations cannot reuse the same ID,
        /// therefore IDs of completed requests can be reused.
        id: u8,
        /// The function's output serialized with rkyv. The format of this 
        /// message will differ with each application.
        /// The macros handle generating the code for this.
        output: Vec<u8>
    }, 
    /// A message from the server with a new event.
    NewEvent {
        /// The event serialized with rkyv. The format of this will differ 
        /// between applications. The macros handle generating the code for this.
        event: Vec<u8>
    }, 
    /// The server updates the connection state.
    StateChange(Vec<(String, Vec<u8>)>),
}
