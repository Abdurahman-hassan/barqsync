# Part 1A: mutation safety

Date: 20 October 2026.

Status: in progress.

## Scope

- DATA-01: prevent same-file truncation, replacement, backup and source removal,
  including hard-link aliases and equivalent paths.
- DATA-04: propagate backup failures to every replacement caller; construct a
  replacement backup atomically so a failure preserves the prior backup.
- SEC-02: enforce `--remove-source-files` and dry-run authorization at both the
  remote acknowledgement boundary and the removal helper.

This part keeps protocol 9 unchanged. Descriptor confinement, replacing
path-based acknowledgements with a sender-owned ledger, and full-resolution
source-removal change tracking belong to parts 1B and 2C. SEC-01, SEC-02 and
DATA-06 must not be described as fully remediated by this part.

## Changes

Implementation and test results will be recorded here before delivery.
