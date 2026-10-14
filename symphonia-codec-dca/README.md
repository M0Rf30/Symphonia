# Symphonia DCA Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_dca-brightgreen?style=for-the-badge" height="22"/>](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-codec-dca)

DTS Coherent Acoustics (DCA, "DTS") decoder for Project Symphonia.

Decodes the backward compatible core substream (all channel arrangements, LFE, 16/20/24-bit
sources, 8-192 kHz core rates, ADPCM, joint intensity coding, VQ, perfect and non-perfect
reconstruction filter banks) together with the XCH, XXCH, X96 and XBR core extensions. Packets in
16-bit and 14-bit, big-endian and little-endian bitstream forms are accepted, as are DTS-HD
packets where the core substream is followed by an extension substream (EXSS).

The lossless (XLL, DTS-HD Master Audio) extension is decoded bit-exactly, including peak bit rate
smoothing, multiple channel sets with embedded downmix, 96 kHz and 192 kHz (two frequency band)
streams, and lossy fallback to the core during recovery. The lossless reconstruction needs the
fixed-point core decoder, which is therefore also implemented (and used for any DTS-HD stream that
carries an XLL component). The low bit rate (LBR, DTS Express) component is not decoded; such
streams are decoded from their core if they have one.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details. See also the NOTICE file for the provenance of the tables and algorithms in this crate.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
