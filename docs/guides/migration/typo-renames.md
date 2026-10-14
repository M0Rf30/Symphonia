# Renames from spelling fixes

Misspelled identifiers were corrected on this fork. Source: commit `fec38ed`
(`symphonia: Wire the new decoders and add elementary stream readers.`).

| Old name                | New name                | Location                                                  | Public? |
|-------------------------|-------------------------|-----------------------------------------------------------|---------|
| `CRM_DESRIPTION`        | `CRM_DESCRIPTION`       | `symphonia_metadata::id3v2::sub_fields`                   | Yes     |
| `BitResevoir`           | `BitReservoir`          | `symphonia-bundle-mp3`, private `layer3` module           | No      |
| `Layer3::resevoir`      | `Layer3::reservoir`     | `symphonia-bundle-mp3`, private `layer3` module           | No      |

Only `CRM_DESCRIPTION` is reachable by downstream crates. The value of the constant is
unchanged (`"DESCRIPTION"`), so tags produced by the ID3v2 CRM frame reader are identical.
The `layer3` module is not exported by `symphonia-bundle-mp3`, so the MP3 renames do not
affect the public API.

## Migrating

Replace the old identifier with the new one:

```rust
// Before
use symphonia_metadata::id3v2::sub_fields::CRM_DESRIPTION;
// After
use symphonia_metadata::id3v2::sub_fields::CRM_DESCRIPTION;
```
