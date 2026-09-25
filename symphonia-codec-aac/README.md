# Symphonia AAC Codec

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_codec_aac-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-codec-aac)

Advanced Audio Coding (AAC) decoder for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Support

This decoder implements the low-complexity (LC) profile as defined in ISO/IEC 14496-3, plus
the HE-AAC v1 (Spectral Band Replication, §4.6.18) and HE-AAC v2 (Parametric Stereo, Annex
8.A) extensions, and multichannel configurations (`channelConfiguration` 1-7 and an explicit
`program_config_element()`).

HE-AAC v1/v2 are ported from the MIT-licensed `oxideav-aac` 0.1.7 crate (see `NOTICE`).
Parametric Stereo is only supported when the whole stream is a single mono `SCE` core
element (the near-universal real-world HE-AAC v2 shape); PS combined with any other channel
configuration is rejected as unsupported rather than guessed at.

Two output-*shape* transitions (sample rate for SBR, channel count for PS) can only be
known once decoding starts rather than at `try_new`, for *implicit* signalling (an ASC/ADTS
header that declares plain LC at the core rate/mono, with the real SBR/PS payload only
discovered in-band): a caller that reads `codec_params()` once at open time will not see the
doubled rate / widened channel count. Such a caller must instead re-check the `AudioSpec` of
the first decoded `AudioBuffer` (`decode_ref`/`last_decoded`) and react to a change, the same
way it must already react to `channelConfiguration == 0`'s deferred channel layout.

## Attribution

Symphonia's AAC decoder was ported and relicensed from the [NihAV](https://nihav.org/) project with permission from the original author, Kostya Shishkov. The first commit with the original decoder is `3aeeb22`.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/pdeljanov/Symphonia/blob/main/CONTRIBUTING.md).
