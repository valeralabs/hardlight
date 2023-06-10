mod client;
mod server;
mod wire;

use std::io::Write;

pub use async_trait;
pub use bytecheck;
pub use client::*;
pub use hardlight_macros::*;
pub use parking_lot;
pub use rkyv;
pub use rkyv_derive;
pub use server::*;
pub use tokio;
pub use tokio_macros;
pub use tokio_tungstenite::tungstenite;
pub use tracing;
use tracing::debug;
pub use wire::*;
use flate2::{write::{DeflateEncoder as Compressor, DeflateDecoder as Decompressor}, Compression};

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

pub(crate) fn inflate(msg: &[u8]) -> Option<Vec<u8>> {
    let mut decompressor = Decompressor::new(vec![]);
    decompressor.write_all(msg).ok()?;
    let out = decompressor.finish().ok();
    debug!("Inflate: {} bytes -> {} bytes", msg.len(), out.as_ref().map(|v| v.len()).unwrap_or(0));
    out
}

pub(crate) fn deflate(msg: &[u8]) -> Option<Vec<u8>> {
    let mut compressor = Compressor::new(vec![], Compression::default());
    compressor.write_all(msg).ok()?;
    let out = compressor.finish().ok();
    debug!("Deflate: {} bytes -> {} bytes", msg.len(), out.as_ref().map(|v| v.len()).unwrap_or(0));
    out
}