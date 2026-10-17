# --delta=blocks vs --delta=cdc (20261017-044906)

Linux aarch64, 5 cpus, 3 interleaved runs, warm cache; fastrsync fastrsync 0.1.0

| Dataset | Where | Command | Mean (s) | Min (s) | Max (s) | Literal | Matched |
|---|---|---|---:|---:|---:|---:|---:|
| B | local | `-a --no-whole-file --delta=blocks` | 2.23 | 1.99 | 2.50 | 55705600 | 2628648960 |
| B | local | `-a --no-whole-file --delta=cdc` | 5.25 | 4.76 | 6.23 | 57281292 | 2627073268 |
| B | local | `-a --no-whole-file --no-clone --delta=blocks` | 4.70 | 4.37 | 5.32 | 55705600 | 2628648960 |
| B | local | `-a --no-whole-file --no-clone --delta=cdc` | 6.71 | 6.53 | 6.84 | 57281292 | 2627073268 |
| B | remote | `-a --delta=blocks` | 2.32 | 2.06 | 2.81 | 55705600 | 2628648960 |
| B | remote | `-a --delta=cdc` | 2.89 | 2.71 | 3.03 | 57281292 | 2627073268 |
| E | local | `-a --no-whole-file --delta=blocks` | 1.05 | 0.74 | 1.28 | 3927377 | 533115720 |
| E | local | `-a --no-whole-file --delta=cdc` | 0.55 | 0.51 | 0.61 | 3927377 | 533115720 |
| E | local | `-a --no-whole-file --no-clone --delta=blocks` | 0.79 | 0.69 | 0.89 | 3927377 | 533115720 |
| E | local | `-a --no-whole-file --no-clone --delta=cdc` | 0.40 | 0.37 | 0.43 | 3927377 | 533115720 |
| E | remote | `-a --delta=blocks` | 0.72 | 0.67 | 0.76 | 3927377 | 533115720 |
| E | remote | `-a --delta=cdc` | 0.77 | 0.65 | 0.89 | 3927377 | 533115720 |
