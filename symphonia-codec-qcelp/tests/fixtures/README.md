# QCELP test fixtures

QCP files (RIFF `QLCM`) from the FFmpeg sample collection, https://samples.ffmpeg.org/A-codecs/ (the
FATE-style `suite/QCP` and `qcp` directories); they are test material of the FFmpeg project, used
here as decoder test vectors only.

| File | Source path | Content |
|---|---|---|
| `clap.qcp` | `suite/QCP/clap.qcp` | variable rate: full, half and eighth rate |
| `0036580847.QCP` | `qcp/0036580847.QCP` | variable rate: full, half, eighth rate; an erased frame |
| `test8.qcp` | `suite/QCP/test8.qcp` | fixed-size packets, full rate |
| `13k_fixed_half.qcp` | `suite/QCP/13k_fixed_half.qcp` | fixed-size half-rate packets |
| `13k_var_half.qcp` | `suite/QCP/13k_var_half.qcp` | variable rate: half and eighth rate |

`*.ref.s16` are the 16-bit little-endian outputs of `ffmpeg -i <file> -f s16le` (FFmpeg 9.0.2,
`qcelp` decoder), for the files whose output does not clip.
