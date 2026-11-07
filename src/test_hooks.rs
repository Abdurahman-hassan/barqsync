//! `BARQSYNC_TEST_*` environment hooks: the integration tests use them to force what
//! can't be raced reliably (a source file that vanishes or shrinks after the scan, a
//! slow control path, an unreachable stream address, a given link speed).
//!
//! They exist only in debug builds and with the `test-hooks` feature. A release binary
//! ignores every one of them: some delete or cut source files, which a variable left in
//! a shell profile on a server must never do.

use std::ffi::OsString;

/// Whether this build reads the hooks. The integration tests check the same condition,
/// which holds for them exactly when it holds for the binary they run.
pub const ENABLED: bool = cfg!(any(debug_assertions, feature = "test-hooks"));

/// The hook `name`, if this build has hooks and it is set.
pub fn var_os(name: &str) -> Option<OsString> {
    if ENABLED { std::env::var_os(name) } else { None }
}

/// [`var_os`] as a string; a value that is not UTF-8 is ignored.
pub fn var(name: &str) -> Option<String> {
    var_os(name).and_then(|v| v.into_string().ok())
}
