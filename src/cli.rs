use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

use crate::add_mod::{self, AssetSource};
use crate::{catalog, convert, database, grep, locres, pak};

/// Default output path for `build-catalog`.
const DEFAULT_CATALOG_OUTPUT: &str = "catalog.json";

/// Default output path for `add-mod`'s database.
const DEFAULT_DATABASE_OUTPUT: &str = "database.h5";

/// Default mod name for `add-mod` when none is given — the base game.
const DEFAULT_MOD_NAME: &str = "vanilla";

#[derive(Parser)]
#[command(name = "fs-assets", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Extract files from a .pak archive.
    Unpak {
        pak: PathBuf,
        /// Only extract paths containing this substring.
        #[arg(long)]
        include: Option<String>,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Convert a single .uasset to another format.
    Convert {
        input: PathBuf,
        #[arg(long, value_enum)]
        to: ConvertFormat,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Build the item/vehicle/structure catalog from an extracted `war/`
    /// directory (expects `War/Content/{Blueprints,Blueprints/Data,
    /// Localization}` under it) or straight from the game's `.pak`.
    BuildCatalog {
        /// Directory the game's pak was already extracted into.
        #[arg(long, required_unless_present = "pak", conflicts_with = "pak")]
        extract_dir: Option<PathBuf>,
        /// The game's `.pak`, read directly: only the catalog inputs are
        /// extracted, into a temporary directory removed afterwards.
        #[arg(long)]
        pak: Option<PathBuf>,
        #[arg(short, long, default_value = DEFAULT_CATALOG_OUTPUT)]
        output: PathBuf,
        /// Keep audio, visual and engine-internal fields in the output.
        #[arg(long)]
        full: bool,
    },
    /// Add one mod's (or the base game's) icons to an icon-template HDF5
    /// database, creating it if it doesn't exist yet. The icons imported are
    /// the ones the existing `--catalog` (see `build-catalog`) references,
    /// recorded under `--mod-name`; re-adding a mod replaces its previous
    /// templates. The catalog is only read.
    AddMod {
        /// Directory the mod's pak was already extracted into.
        #[arg(long, required_unless_present = "pak", conflicts_with = "pak")]
        extract_dir: Option<PathBuf>,
        /// The mod's `.pak`, read directly: only the icons the catalog
        /// references are extracted, into a temporary directory removed
        /// afterwards. Repeat it for a mod split across several paks; a file
        /// several of them contain is taken from the last one.
        #[arg(long)]
        pak: Vec<PathBuf>,
        /// Existing catalog listing the items whose icons are imported.
        #[arg(long)]
        catalog: PathBuf,
        #[arg(long, default_value = DEFAULT_DATABASE_OUTPUT)]
        db: PathBuf,
        #[arg(long, default_value = DEFAULT_MOD_NAME)]
        mod_name: String,
        /// Extraction directory of the base game's own pak, used as a
        /// fallback source for the overlays (subtype icons and the shared
        /// crate-icon texture) this mod's `extract_dir` doesn't itself
        /// provide. Most third-party icon sets only replace item icons and
        /// omit those overlays entirely. Main icons are never taken from it.
        #[arg(long, conflicts_with = "vanilla_pak")]
        vanilla_extract_dir: Option<PathBuf>,
        /// The base game's `.pak`, read directly, as the same icon fallback
        /// as `--vanilla-extract-dir`.
        #[arg(long)]
        vanilla_pak: Option<PathBuf>,
        /// Resolutions to (re)build (comma-separated, e.g. `1080,1440`).
        /// Defaults to every supported resolution; the mod's templates at
        /// the other resolutions are kept.
        #[arg(long, value_delimiter = ',')]
        resolution: Vec<i32>,
    },
    /// Report how many of a catalog's icons some paks contain (main icons,
    /// and the subtype/crate overlays), reading only the pak indexes. Use it
    /// to check a mod before `add-mod`: a mod pak needs main icons; when it
    /// lacks overlays, pass the base game's pak as the vanilla fallback.
    CheckPaks {
        /// Existing catalog listing the items whose icons are looked for.
        #[arg(long)]
        catalog: PathBuf,
        /// A `.pak` to inspect; repeat it for several.
        #[arg(long, required = true)]
        pak: Vec<PathBuf>,
    },
    /// Write the icon of every item in `--catalog` that some paks (or an
    /// extracted directory) provide as `<CodeName>.png`, with the item's
    /// subtype icon drawn over its top-left corner as in game, unless
    /// `--no-subicons` is given.
    ExtractIcons {
        /// Directory a pak was already extracted into.
        #[arg(long, required_unless_present = "pak", conflicts_with = "pak")]
        extract_dir: Option<PathBuf>,
        /// The `.pak` to read the icons from. Repeat it for a mod split
        /// across several paks; a file several of them contain is taken from
        /// the last one.
        #[arg(long)]
        pak: Vec<PathBuf>,
        /// Existing catalog listing the items whose icons are extracted.
        #[arg(long)]
        catalog: PathBuf,
        /// Directory the PNGs are written into.
        #[arg(short, long)]
        output: PathBuf,
        /// Write the bare icons, without the subtype icons drawn over them.
        #[arg(long)]
        no_subicons: bool,
        /// Extraction directory of the base game's own pak, used for the
        /// subtype icons the source doesn't itself provide.
        #[arg(long, conflicts_with_all = ["vanilla_pak", "no_subicons"])]
        vanilla_extract_dir: Option<PathBuf>,
        /// The base game's `.pak`, read directly, as the same subtype-icon
        /// fallback as `--vanilla-extract-dir`.
        #[arg(long, conflicts_with = "no_subicons")]
        vanilla_pak: Option<PathBuf>,
    },
    /// Add, replace, or remove a single icon's template in an existing (or
    /// brand-new) database, independent of any catalog/pak — for one-off
    /// fixes without re-running the full add-mod pipeline. `--replace`
    /// overwrites an existing template; `--remove` deletes it.
    ModifyIcon {
        #[arg(long, default_value = DEFAULT_DATABASE_OUTPUT)]
        db: PathBuf,
        #[arg(long)]
        code: String,
        #[arg(long, value_enum)]
        faction: database::ItemFaction,
        #[arg(long, value_enum)]
        category: database::ItemCategory,
        #[arg(long)]
        crated: bool,
        #[arg(long, default_value = DEFAULT_MOD_NAME)]
        mod_name: String,
        /// Resolutions to modify (comma-separated, e.g. `1080,1440`).
        /// Defaults to every supported resolution when omitted.
        #[arg(long, value_delimiter = ',')]
        resolution: Vec<i32>,
        /// The icon image: a `.png` or a game `.uasset` texture. Required
        /// unless `--remove`.
        #[arg(long)]
        icon: Option<PathBuf>,
        /// Overwrite an existing matching icon instead of erroring.
        #[arg(long)]
        replace: bool,
        /// Remove the matching icon instead of adding one.
        #[arg(long)]
        remove: bool,
    },
    /// Decode a `.locres` file into an editable JSON translation file (see
    /// the README's translation workflow).
    LocresDump {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Optional second `.locres` (typically `en`, the structural
        /// superset) whose (namespace, key) set becomes the output's key
        /// set. For every key present in `--reference` but missing from
        /// `input`, the entry is emitted with `input_text: null` and `text`
        /// pre-filled from the reference file's text. Hash fields always
        /// come from `--reference` when given.
        #[arg(long)]
        reference: Option<PathBuf>,
    },
    /// Re-encode an edited JSON translation file (from `locres-dump`) into a
    /// `.locres`.
    LocresPack {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Pack a directory into a .pak mod file.
    Pack {
        /// Directory whose contents become the pak's file entries. Paths
        /// inside must already mirror the target game's content layout, e.g.
        /// `<input>/War/Content/Localization/Foxhole-Content/pt/Foxhole-Content.locres`.
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Mount point written into the pak header.
        #[arg(long, default_value = "../../../")]
        mount_point: String,
        /// Pak version. Must match (or be compatible with) the base game's
        /// pak; for Foxhole that's V8B.
        #[arg(long, value_enum, default_value = "v8b")]
        version: pak::PakVersion,
    },
    /// Search `.uasset`/`.uexp` files under a directory for a literal
    /// string, checking both ASCII and UTF-16LE encodings — for finding UI
    /// text baked directly into a Blueprint/Widget asset instead of routed
    /// through a `.locres` key.
    GrepStrings {
        pattern: String,
        content_dir: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ConvertFormat {
    Json,
    Png,
}

/// Prints an `add-mod` stage to stderr when it starts and when it ends.
fn print_progress(progress: add_mod::Progress) {
    if progress.done == 0 {
        eprintln!("{}: {} step(s)", progress.stage, progress.total);
    } else if progress.done == progress.total {
        eprintln!("{}: done", progress.stage);
    }
}

/// `error` and its causes on one line, `: `-separated, the way the CLI and
/// the Python bindings report it. A cause whose text the previous message
/// already ends with is skipped: `walkdir`'s errors, for one, repeat their
/// `io::Error` both in their own message and as their cause.
pub fn error_message(error: &anyhow::Error) -> String {
    let mut message = String::new();
    for cause in error.chain() {
        let text = cause.to_string();
        if message.is_empty() {
            message = text;
        } else if !message.ends_with(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
    }
    message
}

/// Prints the library's warnings to stderr as they come.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        eprintln!("{}", record.args());
    }

    fn flush(&self) {}
}

/// Set when Ctrl-C asks the running command to stop. The long commands check
/// it at every progress report and stop there with [`Interrupted`], which
/// unwinds normally so their temporary files are removed.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Whether the running command checks [`INTERRUPTED`]; the short ones don't.
static INTERRUPTIBLE: AtomicBool = AtomicBool::new(false);

/// What the binary's Ctrl-C handler calls (see `main.rs`). Asks the running
/// command to stop at its next progress report and returns `false`, or
/// returns `true` when the process should exit at once instead: on a second
/// Ctrl-C, or while running a command that never reports progress.
pub fn request_interrupt() -> bool {
    !INTERRUPTIBLE.load(Ordering::Relaxed) || INTERRUPTED.swap(true, Ordering::Relaxed)
}

/// The error a command stops with once [`request_interrupt`] asked it to.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// Exit code of a command stopped by Ctrl-C, as shells report SIGINT.
pub const INTERRUPTED_EXIT_CODE: i32 = 130;

/// Fails with [`Interrupted`] once Ctrl-C was pressed.
fn check_interrupted() -> anyhow::Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err(Interrupted.into());
    }
    Ok(())
}

/// Runs the command line `args` (program name first) and returns the process
/// exit code: 0 on success, 1 when the command fails,
/// [`INTERRUPTED_EXIT_CODE`] when Ctrl-C stopped it, and clap's own code for
/// `--help`, `--version` and bad arguments. Never exits the process
/// itself. Ctrl-C is the one [`request_interrupt`] reports.
pub fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_with_interrupt_check(args, &check_interrupted)
}

/// Same as [`run`], with the long commands calling `check_interrupted` at
/// every progress report and stopping with its error, e.g. [`Interrupted`].
/// The Python bindings pass one that runs Python's signal handlers.
pub fn run_with_interrupt_check<I, T>(
    args: I,
    check_interrupted: &dyn Fn() -> anyhow::Result<()>,
) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // Fails when a logger is already installed, e.g. the Python module's when
    // run through `cli_main`, which then keeps receiving the warnings.
    if log::set_logger(&StderrLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // Help and version go to stdout, usage errors to stderr.
            let _ = e.print();
            return e.exit_code();
        }
    };
    INTERRUPTIBLE.store(
        matches!(
            cli.command,
            Commands::Unpak { .. }
                | Commands::BuildCatalog { .. }
                | Commands::AddMod { .. }
                | Commands::ExtractIcons { .. }
                | Commands::Pack { .. }
                | Commands::GrepStrings { .. }
        ),
        Ordering::Relaxed,
    );
    exit_code(run_command(cli.command, check_interrupted))
}

/// The exit code of a command that ended with `result`, reporting its error.
fn exit_code(result: anyhow::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) if e.is::<Interrupted>() => {
            eprintln!("Interrupted");
            INTERRUPTED_EXIT_CODE
        }
        Err(e) => {
            eprintln!("Error: {}", error_message(&e));
            1
        }
    }
}

fn run_command(
    command: Commands,
    check_interrupted: &dyn Fn() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Progress callback of the commands that don't print their progress.
    let interruptible = |_done: usize, _total: usize| check_interrupted();
    match command {
        Commands::Unpak {
            pak,
            include,
            output,
        } => {
            let extracted =
                pak::extract_with_progress(&pak, include.as_deref(), &output, &interruptible)?;
            for path in &extracted {
                println!("{path}");
            }
            eprintln!(
                "extracted {} file(s) to {}",
                extracted.len(),
                output.display()
            );
        }
        Commands::Convert { input, to, output } => match to {
            ConvertFormat::Json => {
                let value = convert::uasset_to_json(&input)?;
                crate::atomic_write::write_json(&output, &value)?;
            }
            ConvertFormat::Png => {
                let png = convert::uasset_to_png(&input)?;
                crate::atomic_write::write_bytes(&output, &png)
                    .with_context(|| format!("writing {}", output.display()))?;
            }
        },
        Commands::BuildCatalog {
            extract_dir,
            pak,
            output,
            full,
        } => {
            let source = match (&extract_dir, &pak) {
                (Some(dir), _) => catalog::pipeline::CatalogSource::ExtractDir(dir),
                (None, Some(pak)) => catalog::pipeline::CatalogSource::Pak(pak),
                (None, None) => anyhow::bail!("either --extract-dir or --pak is required"),
            };
            let stats = catalog::pipeline::build_catalog_to_file(
                source,
                &output,
                full,
                &interruptible,
                &interruptible,
            )?;
            println!(
                "parsed {} stockpilable {} errors {} total_files {}",
                stats.parsed, stats.stockpilable, stats.errors, stats.total_files
            );
        }
        Commands::AddMod {
            extract_dir,
            pak,
            catalog,
            db,
            mod_name,
            vanilla_extract_dir,
            vanilla_pak,
            resolution,
        } => {
            let source =
                AssetSource::from_required_options("--extract-dir", extract_dir, "--pak", pak)?;
            let request = add_mod::AddModRequest {
                source,
                vanilla: AssetSource::from_options(
                    "--vanilla-extract-dir",
                    vanilla_extract_dir,
                    "--vanilla-pak",
                    vanilla_pak.into_iter().collect(),
                )?,
                catalog,
                db,
                mod_name,
                resolutions: resolution,
            };
            let stats = add_mod::add_mod(&request, &|update| {
                print_progress(update);
                check_interrupted()
            })?;

            println!(
                "db: total_items {} items_embedded {} missing_code {} icons_missing {} decode_errors {} overlays_missing {}",
                stats.total_items,
                stats.items_embedded,
                stats.missing_code,
                stats.icons_missing,
                stats.decode_errors,
                stats.overlays_missing
            );
        }
        Commands::CheckPaks { catalog, pak } => {
            let coverage = add_mod::pak_coverage(&catalog, &pak)?;
            println!(
                "main_icons {}/{} overlays {}/{} crate_icon {}",
                coverage.main_icons_found,
                coverage.main_icons_total,
                coverage.overlays_found,
                coverage.overlays_total,
                coverage.has_crate_icon
            );
        }
        Commands::ExtractIcons {
            extract_dir,
            pak,
            catalog,
            output,
            no_subicons,
            vanilla_extract_dir,
            vanilla_pak,
        } => {
            let source =
                AssetSource::from_required_options("--extract-dir", extract_dir, "--pak", pak)?;
            let request = add_mod::ExtractIconsRequest {
                source,
                vanilla: AssetSource::from_options(
                    "--vanilla-extract-dir",
                    vanilla_extract_dir,
                    "--vanilla-pak",
                    vanilla_pak.into_iter().collect(),
                )?,
                catalog,
                output,
                subicons: !no_subicons,
            };
            let stats = add_mod::extract_icons(&request, &|update| {
                print_progress(update);
                check_interrupted()
            })?;
            println!(
                "written {} icons_missing {} decode_errors {} subicons_missing {}",
                stats.written, stats.icons_missing, stats.decode_errors, stats.subicons_missing
            );
        }
        Commands::ModifyIcon {
            db,
            code,
            faction,
            category,
            crated,
            mod_name,
            resolution,
            icon,
            replace,
            remove,
        } => {
            let resolutions = database::resolve_resolutions(&resolution)?;

            let identity = database::pipeline::IconIdentity {
                code: code.clone(),
                faction,
                category,
                crated,
                mod_name,
            };
            database::pipeline::modify_icon_in_database(
                &identity,
                icon.as_deref(),
                &resolutions,
                replace,
                remove,
                &db,
            )?;

            if remove {
                println!("removed icon {code} from {}", db.display());
            } else {
                println!("added/updated icon {code} in {}", db.display());
            }
        }
        Commands::Pack {
            input,
            output,
            mount_point,
            version,
        } => {
            let written = pak::pack_with_progress(
                &input,
                &output,
                &mount_point,
                version.into(),
                &interruptible,
            )?;
            for path in &written {
                println!("{path}");
            }
            eprintln!("packed {} file(s) to {}", written.len(), output.display());
        }
        Commands::LocresDump {
            input,
            output,
            reference,
        } => {
            let namespaces = locres::dump_to_file(&input, reference.as_deref(), &output)?;
            println!("dumped {namespaces} namespace(s) to {}", output.display());
        }
        Commands::LocresPack { input, output } => {
            locres::pack_file(&input, &output)?;
            println!("packed to {}", output.display());
        }
        Commands::GrepStrings {
            pattern,
            content_dir,
        } => {
            let matches =
                grep::search_content_dir_with_progress(&content_dir, &pattern, &interruptible)?;
            for m in &matches {
                println!("{} [{}]", m.path.display(), m.encoding);
            }
            eprintln!(
                "{} match(es) for {pattern:?} under {}",
                matches.len(),
                content_dir.display()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_joins_the_chain_on_one_line() {
        let error = anyhow::Error::from(std::io::Error::other("disk full"))
            .context("writing out.json")
            .context("build-catalog failed");
        assert_eq!(
            error_message(&error),
            "build-catalog failed: writing out.json: disk full"
        );
    }

    #[test]
    fn error_message_skips_a_cause_the_message_already_ends_with() {
        let missing = std::env::temp_dir().join("fs-assets-no-such-dir-for-test");
        let walk_error = walkdir::WalkDir::new(&missing)
            .into_iter()
            .next()
            .unwrap()
            .unwrap_err();
        let message = error_message(&anyhow::Error::from(walk_error));
        assert!(
            message.starts_with("IO error for operation on"),
            "{message}"
        );
        assert_eq!(message.matches("os error 2").count(), 1, "{message}");
    }

    #[test]
    fn run_returns_clap_exit_codes_instead_of_exiting() {
        assert_eq!(run(["fs-assets", "--help"]), 0);
        assert_eq!(run(["fs-assets", "no-such-command"]), 2);
    }

    #[test]
    fn an_interrupted_command_exits_with_130() {
        let interrupted: anyhow::Result<()> = Err(anyhow::Error::from(Interrupted));
        assert_eq!(exit_code(interrupted), INTERRUPTED_EXIT_CODE);
        let wrapped = Err(anyhow::Error::from(Interrupted).context("while extracting"));
        assert_eq!(exit_code(wrapped), INTERRUPTED_EXIT_CODE);
        assert_eq!(exit_code(Err(anyhow::anyhow!("boom"))), 1);
    }

    #[test]
    fn a_failing_interrupt_check_stops_a_long_command_with_130() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.uasset"), b"x").unwrap();
        let args = [
            OsString::from("fs-assets"),
            "grep-strings".into(),
            "x".into(),
            tmp.path().into(),
        ];
        assert_eq!(
            run_with_interrupt_check(args, &|| Err(Interrupted.into())),
            INTERRUPTED_EXIT_CODE
        );
    }

    #[test]
    fn run_returns_1_when_the_command_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert_eq!(
            run([
                OsString::from("fs-assets"),
                "grep-strings".into(),
                "x".into(),
                missing.into(),
            ]),
            1
        );
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("fs-assets").chain(args.iter().copied()))
    }

    #[test]
    fn add_mod_accepts_a_pak_instead_of_an_extract_dir() {
        let cli = parse(&[
            "add-mod",
            "--pak",
            "War.pak",
            "--vanilla-pak",
            "Vanilla.pak",
            "--catalog",
            "catalog.json",
        ])
        .unwrap();
        let Commands::AddMod {
            extract_dir,
            pak,
            vanilla_pak,
            ..
        } = cli.command
        else {
            panic!("expected add-mod");
        };
        assert_eq!(
            AssetSource::from_options("--extract-dir", extract_dir, "--pak", pak).unwrap(),
            Some(AssetSource::Pak(vec!["War.pak".into()]))
        );
        assert_eq!(vanilla_pak, Some(PathBuf::from("Vanilla.pak")));
    }

    #[test]
    fn add_mod_accepts_several_paks_in_order() {
        let cli = parse(&[
            "add-mod",
            "--pak",
            "a.pak",
            "--pak",
            "b.pak",
            "--catalog",
            "catalog.json",
        ])
        .unwrap();
        let Commands::AddMod {
            extract_dir, pak, ..
        } = cli.command
        else {
            panic!("expected add-mod");
        };
        assert_eq!(
            AssetSource::from_options("--extract-dir", extract_dir, "--pak", pak).unwrap(),
            Some(AssetSource::Pak(vec!["a.pak".into(), "b.pak".into()]))
        );
    }

    #[test]
    fn add_mod_accepts_a_comma_separated_resolution_subset() {
        let cli = parse(&[
            "add-mod",
            "--pak",
            "War.pak",
            "--catalog",
            "catalog.json",
            "--resolution",
            "1080,1440",
        ])
        .unwrap();
        let Commands::AddMod { resolution, .. } = cli.command else {
            panic!("expected add-mod");
        };
        assert_eq!(resolution, vec![1080, 1440]);
    }

    #[test]
    fn check_paks_takes_a_catalog_and_several_paks() {
        let cli = parse(&[
            "check-paks",
            "--catalog",
            "catalog.json",
            "--pak",
            "a.pak",
            "--pak",
            "b.pak",
        ])
        .unwrap();
        let Commands::CheckPaks { catalog, pak } = cli.command else {
            panic!("expected check-paks");
        };
        assert_eq!(catalog, PathBuf::from("catalog.json"));
        assert_eq!(pak, vec![PathBuf::from("a.pak"), PathBuf::from("b.pak")]);
        assert!(parse(&["check-paks", "--catalog", "catalog.json"]).is_err());
    }

    #[test]
    fn add_mod_requires_exactly_one_source() {
        assert!(parse(&["add-mod", "--catalog", "c.json"]).is_err());
        assert!(parse(&[
            "add-mod",
            "--catalog",
            "c.json",
            "--pak",
            "a.pak",
            "--extract-dir",
            "dir"
        ])
        .is_err());
        assert!(parse(&[
            "add-mod",
            "--catalog",
            "c.json",
            "--extract-dir",
            "dir",
            "--vanilla-pak",
            "v.pak",
            "--vanilla-extract-dir",
            "v"
        ])
        .is_err());
    }

    #[test]
    fn add_mod_requires_an_existing_catalog_and_never_rewrites_it() {
        assert!(parse(&["add-mod", "--pak", "a.pak"]).is_err());
        assert!(parse(&["add-mod", "--pak", "a.pak", "--catalog", "c.json", "--full"]).is_err());
    }

    #[test]
    fn extract_icons_draws_subicons_unless_told_not_to() {
        let args = [
            "extract-icons",
            "--pak",
            "War.pak",
            "--catalog",
            "catalog.json",
            "-o",
            "icons",
        ];
        let Commands::ExtractIcons { no_subicons, .. } = parse(&args).unwrap().command else {
            panic!("expected extract-icons");
        };
        assert!(!no_subicons);

        let with_flag = [args.as_slice(), &["--no-subicons"]].concat();
        let Commands::ExtractIcons { no_subicons, .. } = parse(&with_flag).unwrap().command else {
            panic!("expected extract-icons");
        };
        assert!(no_subicons);
        assert!(parse(&[with_flag.as_slice(), &["--vanilla-pak", "v.pak"]].concat()).is_err());
    }

    #[test]
    fn build_catalog_accepts_a_pak_instead_of_an_extract_dir() {
        let cli = parse(&["build-catalog", "--pak", "War.pak"]).unwrap();
        let Commands::BuildCatalog {
            extract_dir, pak, ..
        } = cli.command
        else {
            panic!("expected build-catalog");
        };
        assert_eq!(
            AssetSource::from_options(
                "--extract-dir",
                extract_dir,
                "--pak",
                pak.into_iter().collect()
            )
            .unwrap(),
            Some(AssetSource::Pak(vec!["War.pak".into()]))
        );
    }

    #[test]
    fn build_catalog_requires_exactly_one_source() {
        assert!(parse(&["build-catalog"]).is_err());
        assert!(parse(&["build-catalog", "--pak", "a.pak", "--extract-dir", "dir"]).is_err());
    }
}
