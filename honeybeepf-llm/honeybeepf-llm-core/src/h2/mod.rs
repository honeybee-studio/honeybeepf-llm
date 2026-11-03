//! HTTP/2 wire format, read from captured plaintext without taking part in the connection.

pub mod frame;
pub mod hpack;
mod huffman;
