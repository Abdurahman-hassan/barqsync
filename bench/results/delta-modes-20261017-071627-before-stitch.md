# --delta=blocks vs --delta=cdc (20261017-071627)

Darwin arm64, 10 cpus, 3 interleaved runs, warm cache; fastrsync fastrsync 0.1.0

| Dataset | Where | Command | Mean (s) | Min (s) | Max (s) | Literal | Matched |
|---|---|---|---:|---:|---:|---:|---:|
| B | local | `-a --no-whole-file --delta=blocks` | 6.04 | 5.88 | 6.31 | 55705600 | 10681712640 |
| B | local | `-a --no-whole-file --delta=cdc` | 44.13 | 42.54 | 47.27 | 57030283 | 10680387957 |
| B | local | `-a --no-whole-file --no-clone --delta=blocks` | 16.32 | 15.95 | 16.76 | 55705600 | 10681712640 |
| B | local | `-a --no-whole-file --no-clone --delta=cdc` | 79.64 | 77.37 | 84.19 | 57030283 | 10680387957 |
| B | remote | `-a --delta=blocks` | 6.02 | 5.96 | 6.13 | 55705600 | 10681712640 |
| B | remote | `-a --delta=cdc` | 40.18 | 37.92 | 43.29 | 57030283 | 10680387957 |
| E | local | `-a --no-whole-file --delta=blocks` | 3.85 | 3.40 | 4.72 | 86499258 | 2061156575 |
| E | local | `-a --no-whole-file --delta=cdc` | 6.90 | 5.85 | 8.26 | 4079435 | 2143576398 |
| E | local | `-a --no-whole-file --no-clone --delta=blocks` | 3.69 | 3.48 | 3.95 | 86499258 | 2061156575 |
| E | local | `-a --no-whole-file --no-clone --delta=cdc` | 5.83 | 5.39 | 6.43 | 4079435 | 2143576398 |
| E | remote | `-a --delta=blocks` | 2.82 | 2.73 | 2.89 | 86499258 | 2061156575 |
| E | remote | `-a --delta=cdc` | 5.71 | 4.25 | 8.25 | 4079435 | 2143576398 |
