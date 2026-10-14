# Symphonia DSD Format Demuxer

Pure Rust demuxers for DSD (Direct Stream Digital) files.

## Supported formats

- **DSF** (DSD Stream File, Sony v1.01): DSD64 to DSD512 and beyond, 1 to 64 channels, LSB-first
  and MSB-first data, ID3v2 metadata, gapless (the sample count of the `fmt` chunk is exact).
- **DFF** (DSDIFF 1.5, Philips/Sony): uncompressed `DSD ` and DST compressed `DST ` sound data.
  Properties (`FS`, `CHNL`, `CMPR`, `ABSS`, `LSCO`), `DIIN` (`EMID`, `MARK`, `DIAR`, `DITI`),
  `COMT`, `MANF` and `ID3 ` chunks are read to tags and chapters (markers as chapters), and
  metadata may precede or follow the sound data.

## Behaviour

- Packets are 4096 bytes per channel (DSF: one block group); pass-through timestamps and durations
  count 1-bit samples per channel.
- DST packets are one DST frame each (`DSTF`, with the CRC of the `DSTC` chunk if there is one);
  the layout is in the crate documentation. The decoder is in `symphonia-codec-dsd`.
- Seeking is exact to the packet; in DST files the frames are indexed on demand, and the `DSTI`
  chunk is not used.
- Damaged input never panics: sizes are checked against the stream, truncated data delivers the
  complete samples that are present, bad chunk sizes and channel counts are errors, and a stream
  that cannot seek reads up to the sound data.

## Not supported

- DST in DSF (the DSF specification has none).
- The `DSTI` index of DSDIFF is skipped, not used.

## Testing

`cargo test -p symphonia-format-dsd`: unit tests and integration tests that build DSF and DFF
files (valid and damaged, DST, metadata, channel layouts, seeking).

## Specifications

- DSF File Format Specification v1.01 (Sony)
- DSDIFF File Format Specification v1.5 (Philips)

## License

Mozilla Public License 2.0.
