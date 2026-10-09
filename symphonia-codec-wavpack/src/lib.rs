// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

use std::path::{Path, PathBuf};

use symphonia_core::formats::FormatOptions;

mod decoder;
mod reader;

pub use decoder::WavPackDecoder;
pub use reader::WavPackReader;

/// Returns the path of the `.wvc` correction file that accompanies the WavPack file `wv_path`
/// (the same path with the extension `wvc`), if that file exists.
///
/// A hybrid WavPack file (`.wv`) only decodes to its lossy approximation by itself. Together
/// with its `.wvc` file it decodes to the bit-exact lossless audio.
pub fn correction_path(wv_path: &Path) -> Option<PathBuf> {
    let is_upper = wv_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| !ext.is_empty() && ext.chars().all(|c| c.is_ascii_uppercase()));

    let candidates = if is_upper { ["WVC", "wvc"] } else { ["wvc", "WVC"] };

    candidates.iter().map(|ext| wv_path.with_extension(ext)).find(|path| path.is_file())
}

/// Attach the `.wvc` correction file that accompanies the WavPack file `wv_path`, if there is
/// one, to `opts` (as a [`sidecar`](FormatOptions::sidecar) source) so that a probe or a
/// [`WavPackReader`] created with the returned options decodes the hybrid file losslessly.
///
/// `opts` is returned unchanged if `wv_path` has no readable correction file.
///
/// ```no_run
/// use std::path::Path;
/// use symphonia_core::formats::FormatOptions;
///
/// let path = Path::new("album/track.wv");
/// let opts = symphonia_codec_wavpack::with_sibling_correction(path, FormatOptions::default());
/// // Pass `opts` to `Probe::probe` (or `WavPackReader::try_new`) as usual.
/// ```
pub fn with_sibling_correction(wv_path: &Path, opts: FormatOptions) -> FormatOptions {
    match correction_path(wv_path).and_then(|path| std::fs::File::open(path).ok()) {
        Some(file) => opts.sidecar(Box::new(file)),
        None => opts,
    }
}
