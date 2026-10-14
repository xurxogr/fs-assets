use std::ffi::OsString;
use std::path::PathBuf;

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
        faction: FactionArg,
        #[arg(long, value_enum)]
        category: CategoryArg,
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
        version: PakVersionArg,
    },
    /// Search `.uasset`/`.uexp` files under a directory for a literal
    /// string, checking both ASCII/Latin-1 and UTF-16LE encodings — for
    /// finding UI text baked directly into a Blueprint/Widget asset instead
    /// of routed through a `.locres` key.
    GrepStrings {
        pattern: String,
        content_dir: PathBuf,
    },
    /// Print the crate version.
    Version,
}

#[derive(Clone, Copy, ValueEnum)]
enum ConvertFormat {
    Json,
    Png,
}

#[derive(Clone, Copy, ValueEnum)]
enum FactionArg {
    Neutral,
    Colonials,
    Wardens,
}

impl From<FactionArg> for database::ItemFaction {
    fn from(value: FactionArg) -> Self {
        match value {
            FactionArg::Neutral => database::ItemFaction::Neutral,
            FactionArg::Colonials => database::ItemFaction::Colonials,
            FactionArg::Wardens => database::ItemFaction::Wardens,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PakVersionArg {
    V8a,
    V8b,
    V11,
}

impl From<PakVersionArg> for repak::Version {
    fn from(value: PakVersionArg) -> Self {
        match value {
            PakVersionArg::V8a => repak::Version::V8A,
            PakVersionArg::V8b => repak::Version::V8B,
            PakVersionArg::V11 => repak::Version::V11,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum CategoryArg {
    Item,
    Vehicle,
    Shippable,
    Invalid,
}

impl From<CategoryArg> for database::ItemCategory {
    fn from(value: CategoryArg) -> Self {
        match value {
            CategoryArg::Item => database::ItemCategory::Item,
            CategoryArg::Vehicle => database::ItemCategory::Vehicle,
            CategoryArg::Shippable => database::ItemCategory::Shippable,
            CategoryArg::Invalid => database::ItemCategory::Invalid,
        }
    }
}

/// Picks the asset source from a mutually exclusive directory/pak pair of
/// flags, the directory taking precedence (clap already rejects both).
fn asset_source(extract_dir: Option<PathBuf>, paks: Vec<PathBuf>) -> Option<AssetSource> {
    extract_dir
        .map(AssetSource::ExtractDir)
        .or_else(|| (!paks.is_empty()).then_some(AssetSource::Pak(paks)))
}

/// Prints an `add-mod` stage to stderr when it starts and when it ends.
fn print_progress(progress: add_mod::Progress) {
    if progress.done == 0 {
        eprintln!("{}: {} step(s)", progress.stage, progress.total);
    } else if progress.done == progress.total {
        eprintln!("{}: done", progress.stage);
    }
}

pub fn run<I, T>(args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::parse_from(args);

    match cli.command {
        Commands::Unpak {
            pak,
            include,
            output,
        } => {
            let extracted = pak::extract(&pak, include.as_deref(), &output)?;
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
                std::fs::write(&output, serde_json::to_string_pretty(&value)?)?;
            }
            ConvertFormat::Png => {
                let png = convert::uasset_to_png(&input)?;
                std::fs::write(&output, png)?;
            }
        },
        Commands::BuildCatalog {
            extract_dir,
            pak,
            output,
            full,
        } => {
            let (entries, stats) = match asset_source(extract_dir, pak.into_iter().collect()) {
                Some(AssetSource::ExtractDir(dir)) => {
                    catalog::pipeline::build_catalog_from_extract_dir(&dir)?
                }
                Some(AssetSource::Pak(paks)) => match paks.as_slice() {
                    [pak] => catalog::pipeline::build_catalog_from_pak(pak)?,
                    _ => anyhow::bail!("build-catalog reads exactly one --pak"),
                },
                None => anyhow::bail!("either --extract-dir or --pak is required"),
            };
            let file = std::fs::File::create(&output)?;
            let entries = if full {
                entries
            } else {
                catalog::catalog_field_filter::filter_noise(&entries)
            };
            serde_json::to_writer_pretty(file, &entries)?;
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
            let source = asset_source(extract_dir, pak)
                .ok_or_else(|| anyhow::anyhow!("either --extract-dir or --pak is required"))?;
            let request = add_mod::AddModRequest {
                source,
                vanilla: asset_source(vanilla_extract_dir, vanilla_pak.into_iter().collect()),
                catalog,
                db,
                mod_name,
                resolutions: resolution,
            };
            let stats = add_mod::add_mod(&request, &print_progress)?;

            println!(
                "db: total_items {} items_embedded {} missing_code {} icons_missing {} decode_errors {}",
                stats.total_items,
                stats.items_embedded,
                stats.missing_code,
                stats.icons_missing,
                stats.decode_errors
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
            let source = asset_source(extract_dir, pak)
                .ok_or_else(|| anyhow::anyhow!("either --extract-dir or --pak is required"))?;
            let request = add_mod::ExtractIconsRequest {
                source,
                vanilla: asset_source(vanilla_extract_dir, vanilla_pak.into_iter().collect()),
                catalog,
                output,
                subicons: !no_subicons,
            };
            let stats = add_mod::extract_icons(&request)?;
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
                faction: faction.into(),
                category: category.into(),
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
            let written = pak::pack(&input, &output, &mount_point, version.into())?;
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
            let dumped = locres::dump(&input, reference.as_deref())?;
            let file = std::fs::File::create(&output)?;
            serde_json::to_writer_pretty(file, &dumped)?;
            println!(
                "dumped {} namespace(s) to {}",
                dumped.namespaces.len(),
                output.display()
            );
        }
        Commands::LocresPack { input, output } => {
            let file = std::fs::File::open(&input)?;
            let dump_doc: locres::DumpDocument = serde_json::from_reader(file)?;
            let doc = locres::pack(&dump_doc)?;
            locres::write(&doc, &output)?;
            println!("packed to {}", output.display());
        }
        Commands::GrepStrings {
            pattern,
            content_dir,
        } => {
            let matches = grep::search_content_dir(&content_dir, &pattern)?;
            for m in &matches {
                println!("{} [{}]", m.path.display(), m.encoding);
            }
            eprintln!(
                "{} match(es) for {pattern:?} under {}",
                matches.len(),
                content_dir.display()
            );
        }
        Commands::Version => {
            println!("fs-assets {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            asset_source(extract_dir, pak),
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
            asset_source(extract_dir, pak),
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
            asset_source(extract_dir, pak.into_iter().collect()),
            Some(AssetSource::Pak(vec!["War.pak".into()]))
        );
    }

    #[test]
    fn build_catalog_requires_exactly_one_source() {
        assert!(parse(&["build-catalog"]).is_err());
        assert!(parse(&["build-catalog", "--pak", "a.pak", "--extract-dir", "dir"]).is_err());
    }
}
