use barqsync::cli::Options;
use barqsync::{exit, fsops};

fn main() {
    // A relay for parallel streams behind a firewall (started by the client over ssh;
    // see `remote::relay`). Kept out of the options, which travel in the handshake.
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--stream-relay") {
        let targets = args.get(2).and_then(|t| t.to_str()).unwrap_or("");
        std::process::exit(barqsync::remote::relay::run(targets));
    }
    let opts = match Options::parse_from(std::env::args_os()) {
        Ok(o) => o,
        Err(e) => match e.downcast::<clap::Error>() {
            Ok(ce) => {
                let code = if ce.use_stderr() { exit::SYNTAX } else { exit::OK };
                let _ = ce.print();
                std::process::exit(code);
            }
            Err(e) if e.is::<barqsync::cli::InfoHelp>() => {
                print!("{e}");
                std::process::exit(exit::OK);
            }
            Err(e) => {
                eprintln!("barqsync: {e:#}");
                // A rules file that can't be read is a file I/O error for rsync.
                let io = e.chain().any(|c| c.is::<barqsync::filter::RulesFileError>());
                std::process::exit(if io { exit::FILE_IO } else { exit::SYNTAX });
            }
        },
    };
    fsops::handle_interrupts(exit::SIGNAL);
    let code = barqsync::run(opts);
    // An interrupt can arrive after the transfer chose its status, while closing
    // connections or reaping the remote shell. It still determines the process exit.
    std::process::exit(if fsops::interrupted() { exit::SIGNAL } else { code });
}
