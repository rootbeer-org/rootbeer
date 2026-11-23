use rootbeer_unpack::{Format, unpack};
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::process::ExitCode;
use std::thread;

const USAGE: &str = "usage: rb-unpack <dmg|pkg|zip|tar.gz|tar.xz|tar.bz2> <archive> <directory>
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

    match run(format, Path::new(archive), Path::new(directory)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rb-unpack: {archive}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Unpacks into `directory`, which may exist if it's empty, by writing the
/// stream through a pipe the other end lays out.
fn run(format: &str, archive: &Path, directory: &Path) -> Result<(), String> {
    let format = Format::try_from(format.to_string()).map_err(|error| error.to_string())?;
    let archive = File::open(archive).map_err(|error| error.to_string())?;
    let (reader, writer) = io::pipe().map_err(|error| error.to_string())?;

    thread::scope(|scope| {
        let unpacking = scope.spawn(move || unpack(format, archive, BufWriter::new(writer)));
        let laid = rootbeer_tree::materialize(reader, directory).map_err(|error| error.to_string());
        let unpacked = unpacking
            .join()
            .map_err(|_| "unpacking panicked".to_string())?;

        // Either side failing stops the other, so only both say why.
        match (unpacked, laid) {
            (Ok(()), Ok(_)) => Ok(()),
            (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
            (Err(unpacking), Err(laying)) => Err(format!("{laying} ({unpacking})")),
        }
    })
}
