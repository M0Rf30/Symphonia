# Repository Guidelines

## Project Overview

Symphonia is a pure-Rust audio decoding and media demuxing library (MPL-2.0). This repository is
the `M0Rf30/Symphonia` fork, branch `rmpd-next`, which adds many decoders and containers not in
upstream `pdeljanov/Symphonia` (AC-3/E-AC-3, DTS incl. DTS-HD MA, TrueHD/MLP, Speex, Nellymoser,
Musepack SV4–SV8, APE, WavPack, DSD/DST, mp3PRO, FLV, MPEG-PS/TS, raw elementary streams).
Status ratings per codec/format live in `README.md`.

## Architecture & Data Flow

```
MediaSourceStream -> Probe::probe(hint) -> Box<dyn FormatReader> -> next_packet() -> Packet
    -> CodecRegistry::make_audio_decoder(params) -> AudioDecoder::decode(&Packet) -> GenericAudioBufferRef
```

- **`symphonia-core`**: traits and plumbing only. `FormatReader` (`formats/mod.rs`),
  `Scoreable`/`ProbeableFormat`/`Probe` + `support_format!` (`formats/probe.rs`), `AudioDecoder`,
  `AudioDecoderOptions { gapless, verify }` and the `well_known` codec IDs (`codecs/audio.rs`),
  `RegisterableAudioDecoder` + `support_audio_codec!` (`codecs/registry.rs`), `Packet`
  (`packet.rs`), `AudioBuffer`/`GenericAudioBufferRef` (`audio/`), bit readers
  `BitReaderLtr`/`BitReaderRtl` (`io/bit.rs`), errors (`errors.rs`).
- **`symphonia-common`**: code shared across crates (MPEG audio config/PES/framers in `mpeg/`,
  e.g. `mpeg/surround.rs` for AC-3/DTS/TrueHD framing; Xiph and Apple helpers).
- **Codec crates** (`symphonia-codec-*`, `symphonia-bundle-*`) implement `AudioDecoder`;
  **format crates** (`symphonia-format-*`, bundles) implement `FormatReader`. Bundles ship both.
- **Facade `symphonia/`**: one cargo feature per codec/format; `symphonia/src/lib.rs`
  `register_enabled_codecs` / `register_enabled_formats` register them behind `#[cfg(feature)]`.
- **Error contract**: on `Error::DecodeError` consumers drop the packet and continue; on
  `ResetRequired` they rebuild decoders from the new track list; after `seek` all decoders are
  reset. Decoders must leave their buffer cleared after an error.
- **Gapless**: demuxers set `Track::delay`/`padding` and per-packet `trim_start`/`trim_end`;
  decoders trim (`AudioBuffer::trim`) only when `opts.gapless` is true. Examples:
  `symphonia-bundle-mp3/src/demuxer.rs` (LAME tag), `symphonia-format-isomp4/src/demuxer.rs`
  (iTunSMPB/edit lists), `symphonia-format-ogg/src/mappings/opus.rs` (pre-skip).

### Adding a codec
1. Add the ID to `well_known` in `symphonia-core/src/codecs/audio.rs` if missing.
2. New crate `symphonia-codec-<name>/` (`[lints] workspace = true`); add it to root `Cargo.toml`
   `members` and `[workspace.dependencies]` (`default-features = false`).
3. Implement `AudioDecoder` + `RegisterableAudioDecoder` with `support_audio_codec!`
   (template: `symphonia-codec-speex/src/lib.rs`).
4. Facade: feature `name = ["dep:symphonia-codec-name"]` in `symphonia/Cargo.toml`, add to
   `all-codecs`, re-export and register in `symphonia/src/lib.rs`.
5. Map the codec in every container that can carry it (MKV `codecs.rs`, MP4 `stsd`, WAV
   `wave/chunks.rs`, TS/PS stream types, CAF, FLV, Ogg mappings).
6. Update the `README.md` status table, add a crate README (and a `NOTICE` if the code derives
   from another implementation), and add a fuzz target in `symphonia/fuzz/`.

### Adding a format
Implement `Scoreable`, `ProbeableFormat` (`support_format!`) and `FormatReader`
(template: `symphonia-format-es/src/reader.rs`), then add the facade feature and
`probe.register_format::<formats::XReader<'_>>()`.

## Key Directories

| Path | Purpose |
|---|---|
| `symphonia-core/` | Traits, buffers, I/O, probe, registry, DSP (FFT/MDCT) |
| `symphonia-common/` | Shared parsers/framers (MPEG, Xiph, Apple) |
| `symphonia-codec-*/`, `symphonia-bundle-*/` | Decoders (bundles also demux: mp3, flac, ape, musepack) |
| `symphonia-format-*/` | Demuxers (caf, es, flv, isomp4, mkv, mpegps, mpegts, ogg, riff, dsd) |
| `symphonia-metadata/`, `symphonia-tagging/` | ID3v1/v2, APE, Vorbis, RIFF, iTunes tags; tag writing |
| `symphonia/` | Facade crate, examples, cross-crate tests (`tests/`), fuzzing (`fuzz/`) |
| `symphonia-play/` | Reference CLI player (excluded from most CI jobs) |
| `symphonia-check/` | Compares decoding against ffmpeg/flac/mpg123/oggdec |
| `symphonia-testutil/` | Dev-only helpers: `Fnv`, PRNGs, `RMPD_SAMPLES` sample loading |
| `docs/` | Migration guides |

## Development Commands

```sh
cargo check --all --all-features --exclude symphonia-play
cargo clippy --all --all-targets --all-features --exclude symphonia-play -- -D warnings
cargo clippy --all --all-targets -- -D warnings          # default features too
cargo +nightly fmt --all                                  # never stable fmt
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all --all-features --exclude symphonia-play
cargo deny check advisories bans licenses sources
typos                                                     # config: _typos.toml
rustup run 1.85 cargo check --workspace --all-features --exclude symphonia-play   # MSRV
cargo check --manifest-path symphonia/fuzz/Cargo.toml
cargo run --release -p symphonia-play -- file.flac
cargo run --release -p symphonia-check -- --ref ffmpeg file.ac3
```

## Code Conventions & Common Patterns

- Edition 2024, MSRV 1.85. Every crate root has `#![forbid(unsafe_code)]` (never add `unsafe`);
  codec/format crates also start with `#![warn(rust_2018_idioms)]`.
- `clippy::unwrap_used` is a workspace warning and CI uses `-D warnings`: no `unwrap()` in library
  code. Tests/examples start with `#![allow(clippy::unwrap_used)]`.
- Formatting: `rustfmt.toml` (nightly) — 100 columns, `control_brace_style = "ClosingNextLine"`
  (`else` on its own line). Comments are not rewrapped; keep them ≤100 columns yourself.
- Errors: return `decode_error("crate: message")`, `unsupported_error(...)`, `seek_error(...)`,
  `limit_error(...)`, `reset_error()`; prefix messages with the crate short name
  (`"speex: invalid frame"`). Never panic on malformed input.
- Decoders: no per-packet allocation — size buffers in `try_new`, reuse them in `decode`.
- Tables and algorithms come from the specification or a named reference implementation (FFmpeg,
  libspeex, libmpcdec, WavPack, …), never guessed; cite the source in a comment. Where the
  reference deviates from the spec, follow the spec and document the deviation.
- Features are opt-in: workspace deps use `default-features = false`; the facade `default`
  holds royalty-free codecs only. New codecs are not added to `default`.
- Naming: crates `symphonia-{codec,format,bundle}-<name>`; IDs `CODEC_ID_<NAME>`; readers
  `<Name>Reader`, decoders `<Name>Decoder`.
- Commits: `crate: Summary.` subject, body of `- crate: ...` bullets (e.g.
  `aac: Compare SBR fixtures with ffmpeg instead of pinning f32 bits.`).

## Important Files

- `Cargo.toml` — workspace members, shared deps, `[workspace.lints]`.
- `symphonia/Cargo.toml`, `symphonia/src/lib.rs` — features and registration.
- `symphonia-core/src/codecs/audio.rs` — codec IDs, `AudioDecoder`.
- `symphonia-core/src/formats/{mod.rs,probe.rs}` — `FormatReader`, probing.
- `rustfmt.toml`, `clippy.toml`, `deny.toml`, `_typos.toml` — tool configs.
- `.github/workflows/ci.yml` — check, msrv, clippy, test matrix, test-release, docs, fmt, deny, typos.
- `README.md` — codec/format status tables (update when support changes).

## Runtime/Tooling Preferences

- Stable Rust for everything except `cargo +nightly fmt`.
- `cargo-deny`, `typos` and (for fuzzing) `cargo-fuzz` are required tools.
- Tests that use external references need them on `PATH`: `ffmpeg`, `mpg123`, `wvunpack`,
  `mpcdec` (or `MPCDEC_BIN`); they skip when the tool is absent.
- Put scratch files under `~/.cache/` rather than `/tmp`; set `TMPDIR` accordingly when tests
  write temp files (`std::env::temp_dir()`).
- If `sccache` is configured globally and builds look stale, run cargo with `RUSTC_WRAPPER=`.

## Testing & QA

```sh
cargo test --all --exclude symphonia-play                       # CI default features
cargo test --all --all-features --exclude symphonia-play
cargo test --release --all --all-features --exclude symphonia-play
cargo test -p symphonia-codec-aac --test sbr_golden             # one integration test
cargo test -p symphonia-codec-opus --features ogg --release --test golden_hashes
RMPD_SAMPLES=~/rmpd-samples/samples cargo test -p symphonia --all-features --test decode_golden
GOLDEN_PRINT=1 RMPD_SAMPLES=... cargo test -p symphonia-bundle-ape --test ape_golden -- --nocapture
cd symphonia/fuzz && cargo fuzz run decode_any                  # or ./run_all.sh 10
```

- Unit tests inline (`#[cfg(test)] mod tests`); integration tests in `<crate>/tests/`; fixtures in
  `tests/fixtures/` or `tests/data/`, loaded via `env!("CARGO_MANIFEST_DIR")`.
- External corpus: `RMPD_SAMPLES` points at the sample tree; gate with
  `symphonia_testutil::{sample_path, open_sample}` and `let Some(..) else { return; }` so tests
  pass without it.
- Comparing to a reference: lossless or bit-exact decoders (FLAC, APE, WavPack, DSD, Speex vs
  libspeex) may pin FNV hashes (`symphonia_testutil::Fnv`; regenerate with `GOLDEN_PRINT`).
  Float decoders whose output passes through FFT/MDCT (AAC, AC-3, DTS, Opus, Vorbis, MP3) must
  compare against reference PCM with SNR/max-LSB tolerances
  (`symphonia-codec-aac/tests/sbr_golden.rs`): their bits change with `rustfft` SIMD features
  (enabled by workspace feature unification).
- A change is done when clippy (default and all features), nightly fmt check, rustdoc, typos and
  the test suite all pass; decoder changes also need a check against the reference decoder.
- New crates need a fuzz target in `symphonia/fuzz/Cargo.toml` + `fuzz_targets/`.

## Backlog

Known gaps and planned work are GitHub issues on the fork:
<https://github.com/M0Rf30/Symphonia/issues>.

- Before starting, search: `gh issue list -R M0Rf30/Symphonia -S "<keyword>" --state all`, then
  `gh issue view <n> -R M0Rf30/Symphonia`. Reference it in the commit body (`Fixes #<n>`).
- `needs verification` issues are unconfirmed: check the claim first and comment with findings.
- New gaps you are not fixing get an issue, not a TODO.
- Labels: priority `P1`/`P2`/`P3`; area `area: new codec`, `area: codecs`, `area: containers`,
  `area: metadata`, `area: core`, `area: tests`, `area: ci`, `area: docs`; type `codec`,
  `format`, `metadata`, `bug`, `enhancement`, `perf`, `test`, `ci`, `documentation`.
  Pick work with `gh issue list -R M0Rf30/Symphonia -l P1`.
