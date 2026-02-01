mod entropy;
mod decoder;
mod header;
mod silk;
mod toc;
mod packet;

// Re-export the OpusDecoder for use in Symphonia
pub use decoder::OpusDecoder;
