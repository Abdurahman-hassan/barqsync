//! Output the other machine forwards for the client to print is escaped for the client's
//! terminal, like any line that can carry a name. A malicious or compromised server must
//! not be able to put terminal control sequences on the user's screen through a `Log`
//! frame it marks "don't escape", nor through its `Fatal` message.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_barqsync");

/// A fake remote shell: it ignores its arguments and plays a hostile `barqsync --server`
/// on stdin/stdout, reading the client's handshake frame and then sending, by hand
/// (protocol 10, postcard): `HelloAck`, a `Log` line marked not-to-escape and carrying
/// terminal escape sequences, and a `Fatal` message with more.
fn hostile_server(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/usr/bin/perl
use strict; binmode(STDIN); binmode(STDOUT);
sub rd { my ($n)=@_; my $b=''; while (length($b)<$n) { my $r=sysread(STDIN,my $c,$n-length($b)); last if !$r; $b.=$c } $b }
# The client's Hello: u32 LE length, one kind byte, then the body.
my $len = unpack('V', substr(rd(5),0,4)); rd($len);
sub varint { my ($n)=@_; my $o=''; while (1) { my $b=$n & 0x7f; $n>>=7; $o.=chr($b|($n?0x80:0)); last if !$n } $o }
sub frame { my ($b)=@_; pack('V', length($b)) . "\x00" . $b }
# Msg variants: HelloAck=1, Fatal=11, Log=13.
print frame("\x01" . varint(10));                              # HelloAck { version: 10 }
my $t = "\x1b]0;pwned\x07\x1b[2K\rbarqsync: all files OK (fake)\n\x1b[31mred\x1b[0m";
print frame("\x0d\x01\x00" . varint(length($t)) . $t);         # Log { stderr:true, escape:false, text }
my $m = "bye \x1b[31m(x)";
print frame("\x0b" . varint(length($m)) . $m);                 # Fatal { msg }
STDOUT->flush();
rd(1<<30);                                                     # wait for the client to close
"#;
    let path = dir.join("hostile-ssh");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn forwarded_server_output_cannot_write_escape_sequences_to_the_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let shim = hostile_server(tmp.path());
    // A pull, so the client runs the receiver and prints what the "server" forwards. The
    // transport is pinned to ssh (and BARQSYNC_TRANSPORT cleared) so the forwarded Log
    // reaches the receiver loop: under another transport the client would expect a stream
    // offer right after the handshake and reject the hostile Log during setup instead.
    let out = Command::new(BIN)
        .current_dir(tmp.path())
        .args(["-a", "--transport=ssh", "-e"])
        .arg(&shim)
        .args(["--remote-path=barqsync", "fakehost:src/", "dst/"])
        .env("LC_ALL", "C.UTF-8")
        .env_remove("BARQSYNC_TRANSPORT")
        .output()
        .unwrap();
    let mut bytes = out.stdout.clone();
    bytes.extend_from_slice(&out.stderr);

    // No raw terminal-control byte survives: not ESC, BEL or CR from the server's text.
    for b in [0x1b, 0x07, 0x0d] {
        assert!(!bytes.contains(&b), "a control byte {b:#04x} reached the terminal:\n{}", String::from_utf8_lossy(&bytes));
    }
    // It was escaped rather than dropped: the ESC of the Log line shows as rsync's \#033.
    assert!(
        bytes.windows(5).any(|w| w == br"\#033"),
        "the escape sequences were not escaped as \\#033:\n{}",
        String::from_utf8_lossy(&bytes)
    );
}
