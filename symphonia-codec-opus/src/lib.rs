mod celt;
mod decoder;
mod entropy;
mod header;
mod maths;
mod packet;
mod silk;
mod toc;

// Re-export the OpusDecoder for use in Symphonia
pub use decoder::OpusDecoder;
