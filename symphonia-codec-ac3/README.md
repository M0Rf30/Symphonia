# Symphonia AC-3 / E-AC-3 Codec

[![Docs](https://docs.rs/symphonia-codec-ac3/badge.svg)](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-ac3)

Pure Rust AC-3 (ATSC A/52, ETSI TS 102 366) and Enhanced AC-3 (Annex E) decoder for Project Symphonia.

Supported: all channel modes with LFE, block switching, coupling, rematrixing, dynamic range
control, E-AC-3 spectral extension, adaptive hybrid transform, enhanced coupling, dependent
substreams with custom channel maps, transient pre-noise processing, E-AC-3 reduced sample rates,
and byte-swapped AC-3. Programme 0 of multi-programme E-AC-3 is decoded. AC-3 `bsid` 9 and 10 are
muted as the specification requires. Output is planar `f32`. Downmixing is not performed.

Verified against FFmpeg (with its known deviations from ETSI TS 102 366 corrected) except for
transient pre-noise processing and reduced sample rates, which no reference decoder implements.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.
See `NOTICE` for the origin of the algorithms and tables.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contributing Guidelines](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/CONTRIBUTING.md).
