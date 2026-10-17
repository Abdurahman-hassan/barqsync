# --io-uring (20261017-122318)

Remote push (fake ssh) of 20,000 x 4 KiB files; 5 runs. Kernel 6.19.7-200.fc43.aarch64.

## onto tmpfs (/dst/uring)

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `normal writes` | 128.5 ± 13.9 | 114.0 | 149.1 | 1.00 |
| `io_uring batches` | 133.9 ± 8.5 | 124.4 | 147.1 | 1.04 ± 0.13 |

- syscalls normal: 269069 (read 41354, close 40069, openat 40065, statx 40028, utimensat 20021, renameat 20000, ftruncate 20000, )
- syscalls --io-uring: 190611 (read 41341, statx 40028, close 20078, openat 20065, utimensat 20021, renameat 20000, fchown 20000, )

## onto xfs (/data/uring)

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `normal writes` | 689.1 ± 85.7 | 578.4 | 791.2 | 2.80 ± 0.49 |
| `io_uring batches` | 246.4 ± 29.9 | 204.6 | 283.4 | 1.00 |

- syscalls normal: 269159 (read 41348, close 40068, openat 40065, statx 40028, utimensat 20021, renameat 20000, ftruncate 20000, )
- syscalls --io-uring: 190236 (read 41338, statx 40028, close 20079, openat 20066, utimensat 20021, renameat 20000, fchown 20000, )

