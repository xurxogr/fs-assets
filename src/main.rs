fn main() -> anyhow::Result<()> {
    fs_assets::cli::run(std::env::args_os())
}
