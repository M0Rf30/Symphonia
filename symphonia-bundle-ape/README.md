# Symphonia APE (Monkey's Audio) Codec

[![Docs](https://docs.rs/symphonia-bundle-ape/badge.svg)](https://docs.rs/symphonia-bundle-ape)

APE (Monkey's Audio) demuxer and decoder for Project Symphonia.

**Note:** This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

The `src/mac` module is derived from the [ape-decoder](https://crates.io/crates/ape-decoder) crate (Copyright (c) 2026 ombs.io, licensed under MIT OR Apache-2.0) and retains its license files and attribution; see `src/mac/NOTICE`.

## Acknowledgements

 * [Monkey's Audio SDK](https://monkeysaudio.com/), for the reference implementation
 * [ape-decoder](https://crates.io/crates/ape-decoder), the pure Rust APE header and frame decoding library that the `src/mac` module was vendored from

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/pdeljanov/Symphonia/tree/master/CONTRIBUTING.md).
