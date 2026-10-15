# Symphonia AC-3 / E-AC-3 Codec

[![Docs](https://docs.rs/symphonia-codec-ac3/badge.svg)](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-ac3)

Pure Rust AC-3 (ATSC A/52, ETSI TS 102 366) and Enhanced AC-3 (Annex E) decoder for Project Symphonia.

Supported: all channel modes with LFE, block switching, coupling, rematrixing, dynamic range
control, E-AC-3 spectral extension, adaptive hybrid transform, enhanced coupling, dependent
substreams with custom channel maps, transient pre-noise processing, E-AC-3 reduced sample rates,
and byte-swapped AC-3. Programme 0 of multi-programme E-AC-3 is decoded. AC-3 `bsid` 9 and 10 are
muted as the specification requires. Output is planar `f32`.

Down-mix to stereo (Lo/Ro or Lt/Rt) or mono (ETSI TS 102 366 clause 7.8) is selected with
`AudioDecoderOptions::downmix`, using the `cmixlev`/`surmixlev` and Annex D/E mixing metadata of
the stream. The default is to output every channel.

Verified against FFmpeg (with its known deviations from ETSI TS 102 366 corrected) with committed
reference PCM fixtures. Two features have no reference decoder: transient pre-noise processing
(FFmpeg parses and discards the data; checked against the specification's equations) and E-AC-3
reduced sample rates (FFmpeg rejects them; only the framing is checked). In 1+1 mode the LFE
channel follows `dynrng`, whereas FFmpeg applies `dynrng2` to it; the mono down-mix is
`(Lo + Ro) / 2` as specified, whereas FFmpeg scales by 0.707.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.
See `NOTICE` for the origin of the algorithms and tables.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contributing Guidelines](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/CONTRIBUTING.md).
