mod client;
mod server;
mod wire;

pub use client::*;
pub use hardlight_macros::*;
pub use server::*;
pub use tokio_tungstenite::tungstenite;
pub use wire::*;

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub struct Topic(Vec<u8>);

impl Into<Vec<u8>> for Topic {
    fn into(self) -> Vec<u8> {
        self.0
    }
}

impl Into<Topic> for Vec<u8> {
    fn into(self) -> Topic {
        Topic(self)
    }
}

impl Into<Topic> for &str {
    fn into(self) -> Topic {
        Topic(self.as_bytes().to_vec())
    }
}

impl Into<Topic> for String {
    fn into(self) -> Topic {
        Topic(self.as_bytes().to_vec())
    }
}
