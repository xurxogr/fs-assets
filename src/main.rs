use std::process::ExitCode;

fn main() -> ExitCode {
    match fs_assets::cli::run(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", fs_assets::cli::error_message(&e));
            ExitCode::FAILURE
        }
    }
}
