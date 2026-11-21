use rootbeer_unpack::{Format, unpack};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: rb-unpack <dmg|pkg|zip|tar.bz2> <archive> <directory>
       rb-unpack --version";

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments == ["--version"] {
        println!("rb-unpack {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let [format, archive, directory] = arguments.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let result = format
        .parse::<Format>()
        .and_then(|format| unpack(format, Path::new(archive), Path::new(directory)));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rb-unpack: {archive}: {error}");
            ExitCode::FAILURE
        }
    }
}
