# MPEG-H 3D Audio work in progress

These files are not part of the workspace and are not compiled. They were moved here from the
orphan directory `symphonia-codec-mpegh/` (no `Cargo.toml`, not a workspace member) in the
follow-up to issues #113 and #114; the work they belong to is tracked in issue #97.

## Origin

* `arith.rs`: spectral 2-tuple arithmetic decoder of the USAC/MPEG-H FD core, ported from the
  `libArithCoding/src/ac_arith_coder.cpp` of the FDK-AAC library (Fraunhofer).
* `bits.rs`: bit reader used by the arithmetic decoder.
* `fxp.rs`: fixed-point helpers.

**Licence: to be checked.** FDK-AAC is distributed under the Fraunhofer FDK AAC Codec Library
licence, which is not MPL-2.0 compatible as far as this project has established. Until the licence
question is settled, none of this code may be merged into a crate, and it must not be extended by
copying more of the FDK sources.

## Status

Incomplete and unverified:

* `arith.rs` imports `crate::tables` (`ARI_PK`, `ARI_LSB2`, `ARI_MERGED_HASH_PS`,
  `ARI_MERGED_HASH_PS_EXT`), which was never committed, so it does not build.
* There is no crate manifest, no USAC core, no MPEG-H element layer and no reference decoder or
  test streams.

See issue #97 for the plan.
