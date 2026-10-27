use std::process::ExitCode;

use fs_assets::cli;

fn main() -> ExitCode {
    // The first Ctrl-C asks the running command to stop at its next progress
    // report, so it can remove its temporary files; a second one, or one
    // during a command that never reports progress, quits at once.
    let _ = ctrlc::set_handler(|| {
        if cli::request_interrupt() {
            std::process::exit(cli::INTERRUPTED_EXIT_CODE);
        }
        eprintln!("Stopping… press Ctrl-C again to quit now");
    });
    // Exit codes are 0, 1, 2 (clap) or 130, all of which fit in a u8.
    ExitCode::from(cli::run(std::env::args_os()) as u8)
}
