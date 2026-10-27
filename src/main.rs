use std::process::ExitCode;

fn main() -> ExitCode {
    // Exit codes are 0, 1 or clap's 2, all of which fit in a u8.
    ExitCode::from(fs_assets::cli::run(std::env::args_os()) as u8)
}
