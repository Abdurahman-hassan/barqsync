//! `fastrsync --server`: the remote end, started by the client over ssh. Everything it
//! needs (role, paths, options) arrives in the `Hello` frame on stdin; stdout carries
//! only protocol frames, so all output is forwarded to the client as `Log` frames.

use super::{receiver, sender, streams};
use crate::exit;
use crate::output::Output;
use crate::proto::{Conn, Frame, MAGIC, Msg, VERSION};

pub fn run() -> i32 {
    let mut conn = Conn::start(std::io::stdin(), std::io::stdout(), false);
    let (server_is_sender, opts) = match conn.recv() {
        Ok(Frame::Msg(Msg::Hello { magic, version, server_is_sender, opts })) => {
            if magic != MAGIC || version != VERSION {
                let msg = format!("protocol version mismatch: client speaks {version}, server speaks {VERSION}");
                let _ = conn.msg(Msg::Fatal { msg });
                let _ = conn.finish();
                return exit::INCOMPATIBLE;
            }
            (server_is_sender, opts)
        }
        _ => {
            eprintln!("fastrsync --server: expected a fastrsync client handshake on stdin");
            return exit::PROTOCOL;
        }
    };
    conn.set_compress(opts.compress);
    if conn.msg(Msg::HelloAck { version: VERSION }).is_err() {
        return exit::PROTOCOL;
    }
    if let Err(e) = streams::setup_server(&mut conn, opts.streams, server_is_sender) {
        eprintln!("fastrsync --server: setting up parallel streams failed: {e}");
        let _ = conn.msg(Msg::Fatal { msg: format!("parallel stream setup failed: {e}") });
        let _ = conn.finish();
        return exit::PROTOCOL;
    }
    let fwd = conn.out.clone();
    // In a push the items come from this (receiving) side but describe what the
    // client sent, so they are shown as rsync's sender does (`<f...`).
    let out = Output::forwarding(
        &opts,
        !server_is_sender,
        Box::new(move |stderr, escape, text| {
            let _ = fwd.msg(Msg::Log { stderr, escape, text: text.to_vec() });
        }),
    );
    let code = if server_is_sender {
        sender::run(&conn, &opts, &opts.paths, &out).0
    } else {
        match opts.paths.first() {
            Some(dest) => receiver::run(&conn, &opts, dest, &out).0,
            None => {
                let _ = conn.msg(Msg::Fatal { msg: "no destination given to the server".into() });
                exit::SYNTAX
            }
        }
    };
    drop(out);
    let _ = conn.finish();
    code
}
