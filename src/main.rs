use fastrsync::cli::Options;
use fastrsync::{exit, fsops};

fn main() {
    let opts = match Options::parse_from(std::env::args_os()) {
        Ok(o) => o,
        Err(e) => match e.downcast::<clap::Error>() {
            Ok(ce) => {
                let code = if ce.use_stderr() { exit::SYNTAX } else { exit::OK };
                let _ = ce.print();
                std::process::exit(code);
            }
            Err(e) => {
                eprintln!("fastrsync: {e:#}");
                std::process::exit(exit::SYNTAX);
            }
        },
    };
    let _ = ctrlc::set_handler(|| fsops::on_interrupt(exit::SIGNAL));
    std::process::exit(fastrsync::run(opts));
}
