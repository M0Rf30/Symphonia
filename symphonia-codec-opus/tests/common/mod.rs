#![allow(clippy::unwrap_used)] // test/example code: a panic is the failure signal
#![allow(dead_code)] // shared by several test crates, each using a subset
pub mod opus_compare;
pub mod opus_demo;
