# Symphonia AU, VOC, IFF/8SVX and raw PCM demuxers

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_format_sound-brightgreen?style=for-the-badge" height="22"/>](https://github.com/M0Rf30/Symphonia/tree/rmpd-next/symphonia-format-sound)

Pure Rust demuxers for the simple sampled-sound file formats of Project Symphonia:

| Feature   | Reader         | Format                                    |
|-----------|----------------|-------------------------------------------|
| `au`      | `AuReader`     | Sun/NeXT AU (`.au`, `.snd`)               |
| `voc`     | `VocReader`    | Creative Voice File (`.voc`)              |
| `iff`     | `IffReader`    | IFF 8SVX and 16SV (`.iff`, `.8svx`)       |
| `raw-pcm` | `RawPcmReader` | headerless PCM, constructed with parameters |

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/M0Rf30/Symphonia/blob/rmpd-next/CONTRIBUTING.md).
