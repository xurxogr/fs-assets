//! PyO3 bindings, one thin wrapper per CLI command (see `src/cli.rs`), so
//! the Python package exposes the exact same operations as the native
//! binary. Each function does its own file I/O (writing `catalog.json`/
//! `database.h5` etc.) exactly like the CLI does, and returns build
//! statistics (or the raw conversion output) as native Python objects.

use std::path::PathBuf;
use std::sync::Mutex;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use crate::add_mod::{AddModRequest, AssetSource, ExtractIconsRequest, Progress};
use crate::{catalog, convert, database, grep, locres};

/// Raises `e` as the `OSError` subclass matching the I/O error that caused it
/// (`FileNotFoundError` for a missing path, `PermissionError`, ...), or as a
/// `RuntimeError` when no I/O error did.
fn to_py_err(e: anyhow::Error) -> PyErr {
    let message = crate::cli::error_message(&e);
    match e
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
    {
        Some(io) => std::io::Error::new(io.kind(), message).into(),
        None => PyErr::new::<PyRuntimeError, _>(message),
    }
}

/// Converts a `serde_json::Value` into a native Python object (dict/list/
/// str/...) by round-tripping it through the stdlib `json` module.
fn json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    let json_str =
        serde_json::to_string(value).map_err(|e| PyErr::new::<PyRuntimeError, _>(e.to_string()))?;
    let json_module = PyModule::import(py, "json")?;
    let result = json_module.call_method1("loads", (json_str,))?;
    Ok(result.unbind())
}

/// Parses `value` as one of `T`'s variants by the name the CLI takes,
/// ignoring case and surrounding whitespace; `what` names it in the error.
fn parse_choice<T: clap::ValueEnum>(what: &str, value: &str) -> PyResult<T> {
    T::from_str(value.trim(), true).map_err(|_| {
        let expected: Vec<String> = T::value_variants()
            .iter()
            .filter_map(|v| v.to_possible_value())
            .map(|v| v.get_name().to_owned())
            .collect();
        PyErr::new::<PyValueError, _>(format!(
            "invalid {what} '{value}': expected one of {}",
            expected.join(", ")
        ))
    })
}

/// Hands the library's warnings to Python's `logging`, under the `fs_assets`
/// logger, so callers can filter, capture or silence them.
struct PythonLogger;

impl log::Log for PythonLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let level = match record.level() {
            log::Level::Error => 40,
            log::Level::Warn => 30,
            log::Level::Info => 20,
            log::Level::Debug => 10,
            log::Level::Trace => 5,
        };
        Python::attach(|py| {
            // A failing handler mustn't fail the run that is warning.
            let _ = PyModule::import(py, "logging")
                .and_then(|logging| logging.call_method1("getLogger", ("fs_assets",)))
                .and_then(|logger| logger.call_method1("log", (level, record.args().to_string())));
        });
    }

    fn flush(&self) {}
}

/// An optional Python `(stage, done, total)` progress callable, called from
/// Rust code that runs with the GIL released. Each report also runs pending
/// signal handlers, so Ctrl-C raises `KeyboardInterrupt` even without a
/// callable. The first exception the callable or a handler raises is kept and
/// fails that report, which stops the work; [`PyProgress::finish`] re-raises it.
struct PyProgress {
    callback: Option<Py<PyAny>>,
    error: Mutex<Option<PyErr>>,
}

impl PyProgress {
    fn new(callback: Option<Py<PyAny>>) -> Self {
        Self {
            callback,
            error: Mutex::new(None),
        }
    }

    /// Calls the callable, if any, and the pending signal handlers
    /// (re-acquiring the GIL), failing once either has raised.
    fn report(&self, stage: &str, done: usize, total: usize) -> anyhow::Result<()> {
        let mut error = self.error.lock().unwrap_or_else(|e| e.into_inner());
        if error.is_none() {
            *error = Python::attach(|py| {
                if let Some(callback) = &self.callback {
                    callback.call1(py, (stage, done, total))?;
                }
                py.check_signals()
            })
            .err();
        }
        match *error {
            Some(_) => Err(anyhow::anyhow!("stopped by a Python exception")),
            None => Ok(()),
        }
    }

    /// Re-raises the exception the callable raised, if any.
    fn finish(self) -> PyResult<()> {
        match self.error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// One `.pak` path, or several in override order (later ones win). Like
/// every path argument here, each is a `str` or an `os.PathLike`.
#[derive(FromPyObject)]
enum PakPaths {
    One(PathBuf),
    Many(Vec<PathBuf>),
}

impl PakPaths {
    fn into_paths(self) -> Vec<PathBuf> {
        match self {
            PakPaths::One(path) => vec![path],
            PakPaths::Many(paths) => paths,
        }
    }
}

/// The source a directory argument named `dir_name` and a pak argument named
/// `pak_name` pick, if any, as [`AssetSource::from_options`].
fn asset_source(
    dir_name: &str,
    dir: Option<PathBuf>,
    pak_name: &str,
    paks: Vec<PathBuf>,
) -> PyResult<Option<AssetSource>> {
    AssetSource::from_options(dir_name, dir, pak_name, paks).map_err(value_err)
}

/// Like [`asset_source`], for a source that is required.
fn required_asset_source(
    dir_name: &str,
    dir: Option<PathBuf>,
    pak_name: &str,
    paks: Vec<PathBuf>,
) -> PyResult<AssetSource> {
    AssetSource::from_required_options(dir_name, dir, pak_name, paks).map_err(value_err)
}

fn value_err(e: anyhow::Error) -> PyErr {
    PyErr::new::<PyValueError, _>(e.to_string())
}

/// The resolutions a `resolution` argument names (every supported one when
/// it's omitted), or `ValueError` for one that isn't supported.
fn resolutions_arg(resolution: Option<Vec<i32>>) -> PyResult<Vec<i32>> {
    database::resolve_resolutions(&resolution.unwrap_or_default()).map_err(value_err)
}

/// `ValueError` unless `value`, the `what` argument, can name a template.
fn name_arg(what: &str, value: &str) -> PyResult<()> {
    database::check_template_name(what, value).map_err(value_err)
}

/// Extract files from a .pak archive.
///
/// Args:
///     pak: Path to the .pak archive.
///     output: Directory to extract into.
///     include: Only extract paths containing this substring.
///     progress: Optional callable invoked as `progress(stage, done, total)`
///         with stage "extracting", over the extracted files.
///
/// Returns:
///     List of extracted file paths (relative to `output`).
#[pyfunction]
#[pyo3(signature = (pak, output, *, include=None, progress=None))]
fn unpak(
    py: Python<'_>,
    pak: PathBuf,
    output: PathBuf,
    include: Option<&str>,
    progress: Option<Py<PyAny>>,
) -> PyResult<Vec<String>> {
    let progress = PyProgress::new(progress);
    let result = py.detach(|| {
        crate::pak::extract_with_progress(&pak, include, &output, &|done, total| {
            progress.report("extracting", done, total)
        })
    });
    progress.finish()?;
    result.map_err(to_py_err)
}

/// Convert a single .uasset to JSON.
///
/// Args:
///     path: Path to the .uasset file.
///
/// Returns:
///     Parsed asset contents as a dict.
#[pyfunction]
fn convert_to_json(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
    let value = py
        .detach(|| convert::uasset_to_json(&path))
        .map_err(to_py_err)?;
    json_to_py(py, &value)
}

/// Convert a single .uasset texture to a PNG.
///
/// Args:
///     path: Path to the .uasset file.
///
/// Returns:
///     PNG-encoded image bytes.
#[pyfunction]
fn convert_to_png(py: Python<'_>, path: PathBuf) -> PyResult<Vec<u8>> {
    py.detach(|| convert::uasset_to_png(&path))
        .map_err(to_py_err)
}

/// Build the item/vehicle/structure catalog from an extracted pak directory
/// or straight from the game's `.pak`, and write it to `output` as JSON.
///
/// Args:
///     extract_dir: Root of an extracted pak (contains `War/Content/...`).
///         Exactly one of `extract_dir` and `pak` is required.
///     output: Path to write the catalog JSON to.
///     full: Keep audio, visual and engine-internal fields in the output.
///     pak: The game's `.pak`, read directly: only the catalog inputs are
///         extracted, into a temporary directory removed afterwards.
///     progress: Optional callable `(stage, done, total)` called as the build
///         advances: `stage` is `extracting_catalog_inputs` (only with
///         `pak`) or `building_catalog`; each stage reports `done == 0`
///         when it starts and `done == total` when it ends. The GIL is
///         released while the build works. If the callable raises, it isn't
///         called again and its exception is re-raised once the build ends.
///
/// Returns:
///     Dict of build statistics (total_files, parsed, stockpilable, errors).
#[pyfunction]
#[pyo3(
    signature = (
        extract_dir=None, output=PathBuf::from("catalog.json"), full=false, *, pak=None,
        progress=None,
    ),
    text_signature = "(extract_dir=None, output='catalog.json', full=False, *, pak=None, progress=None)"
)]
fn build_catalog(
    py: Python<'_>,
    extract_dir: Option<PathBuf>,
    output: PathBuf,
    full: bool,
    pak: Option<PathBuf>,
    progress: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    let paks = pak.into_iter().collect();
    let source = required_asset_source("extract_dir", extract_dir, "pak", paks)?;
    let source = match &source {
        AssetSource::ExtractDir(dir) => catalog::pipeline::CatalogSource::ExtractDir(dir),
        // Built from the single `pak` argument, so it holds exactly one.
        AssetSource::Pak(paks) => catalog::pipeline::CatalogSource::Pak(&paks[0]),
    };
    let progress = PyProgress::new(progress);
    let result = py.detach(|| {
        catalog::pipeline::build_catalog_to_file(
            source,
            &output,
            full,
            &|done, total| progress.report("extracting_catalog_inputs", done, total),
            &|done, total| progress.report("building_catalog", done, total),
        )
    });
    progress.finish()?;
    let stats = result.map_err(to_py_err)?;

    let stats_json = serde_json::json!({
        "total_files": stats.total_files,
        "parsed": stats.parsed,
        "stockpilable": stats.stockpilable,
        "errors": stats.errors,
    });
    json_to_py(py, &stats_json)
}

/// Add one mod's (or the base game's) icons to an icon-template HDF5
/// database, creating it if it doesn't exist yet. The icons imported are the
/// ones the existing `catalog` (see `build_catalog`) references, recorded
/// under `mod_name`; re-adding a mod replaces its previous templates. The
/// catalog is only read.
///
/// Args:
///     extract_dir: Root of the mod's extracted pak. Exactly one of
///         `extract_dir` and `pak` is required.
///     pak: The mod's `.pak` (or a list of them, later ones overriding
///         earlier ones), read directly: only the icons the catalog
///         references are extracted, into a temporary directory removed
///         afterwards.
///     catalog: Existing catalog JSON listing the items whose icons are
///         imported.
///     db: Path to the icon-template HDF5 database to create/merge into.
///     mod_name: Name recorded against every template added by this call.
///     vanilla_extract_dir: Extraction directory of the base game's own pak,
///         used as a fallback source for the overlays (subtype icons and the
///         shared crate-icon texture) the mod doesn't itself provide. Main
///         icons are never taken from it.
///     vanilla_pak: The base game's `.pak`, read directly, as the same
///         fallback. At most one of `vanilla_extract_dir` and `vanilla_pak`.
///     resolution: Resolutions (vertical pixel heights) to rebuild; every
///         supported resolution if omitted. The mod's templates at the other
///         resolutions are kept.
///     progress: Optional callable `(stage, done, total)` called as the run
///         advances: `stage` is one of `extracting_icons`,
///         `extracting_vanilla_icons` or `building_templates`; each stage
///         reports `done == 0` when it starts and `done == total` when it
///         ends. The GIL is released while the run works, so other Python
///         threads (e.g. a GUI) keep running. If the callable raises, it isn't
///         called again and its exception is re-raised once the run finishes.
///
/// Returns:
///     Dict of database build statistics.
#[pyfunction]
#[pyo3(
    signature = (
        extract_dir=None, *, catalog, pak=None, db=PathBuf::from("database.h5"),
        mod_name="vanilla", vanilla_extract_dir=None, vanilla_pak=None, resolution=None,
        progress=None,
    ),
    text_signature = "(extract_dir=None, *, catalog, pak=None, db='database.h5', mod_name='vanilla', vanilla_extract_dir=None, vanilla_pak=None, resolution=None, progress=None)"
)]
#[allow(clippy::too_many_arguments)]
fn add_mod(
    py: Python<'_>,
    extract_dir: Option<PathBuf>,
    catalog: PathBuf,
    pak: Option<PakPaths>,
    db: PathBuf,
    mod_name: &str,
    vanilla_extract_dir: Option<PathBuf>,
    vanilla_pak: Option<PathBuf>,
    resolution: Option<Vec<i32>>,
    progress: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    name_arg("mod_name", mod_name)?;
    let resolutions = resolutions_arg(resolution)?;
    let paks = pak.map(PakPaths::into_paths).unwrap_or_default();
    let source = required_asset_source("extract_dir", extract_dir, "pak", paks)?;
    let request = AddModRequest {
        source,
        vanilla: asset_source(
            "vanilla_extract_dir",
            vanilla_extract_dir,
            "vanilla_pak",
            vanilla_pak.into_iter().collect(),
        )?,
        catalog,
        db,
        mod_name: mod_name.to_string(),
        resolutions,
    };
    let progress = PyProgress::new(progress);
    let result = py.detach(|| {
        crate::add_mod::add_mod(&request, &|update: Progress| {
            progress.report(update.stage.as_str(), update.done, update.total)
        })
    });
    progress.finish()?;
    let stats = result.map_err(to_py_err)?;

    let stats_json = serde_json::json!({
        "total_items": stats.total_items,
        "items_embedded": stats.items_embedded,
        "missing_code": stats.missing_code,
        "icons_missing": stats.icons_missing,
        "decode_errors": stats.decode_errors,
        "overlays_missing": stats.overlays_missing,
    });
    json_to_py(py, &stats_json)
}

/// Add, replace, or remove a single icon's template in an existing (or
/// brand-new) database, independent of any catalog/pak.
///
/// Args:
///     db: Path to the icon-template HDF5 database to create/modify.
///     code: Item code the template is keyed on.
///     faction: One of "neutral", "colonials", "wardens".
///     category: One of "item", "vehicle", "shippable", "invalid".
///     crated: Whether this is the crated variant of the item.
///     mod_name: Name recorded against the template.
///     resolution: Resolutions (vertical pixel heights) to modify; every
///         supported resolution if omitted.
///     icon: Path to the icon image, a .png or a game .uasset texture;
///         required unless `remove`.
///     replace: Overwrite an existing matching template instead of erroring.
///     remove: Remove the matching template instead of adding one.
#[pyfunction]
#[pyo3(
    signature = (
        db=PathBuf::from("database.h5"), *, code, faction, category, crated=false,
        mod_name="vanilla", resolution=None, icon=None, replace=false, remove=false,
    ),
    text_signature = "(db='database.h5', *, code, faction, category, crated=False, mod_name='vanilla', resolution=None, icon=None, replace=False, remove=False)"
)]
#[allow(clippy::too_many_arguments)]
fn modify_icon(
    py: Python<'_>,
    db: PathBuf,
    code: &str,
    faction: &str,
    category: &str,
    crated: bool,
    mod_name: &str,
    resolution: Option<Vec<i32>>,
    icon: Option<PathBuf>,
    replace: bool,
    remove: bool,
) -> PyResult<()> {
    name_arg("code", code)?;
    name_arg("mod_name", mod_name)?;
    if remove && (replace || icon.is_some()) {
        return Err(PyValueError::new_err(
            "remove can't be combined with replace or icon",
        ));
    }
    if !remove && icon.is_none() {
        return Err(PyValueError::new_err(
            "icon is required unless remove is set",
        ));
    }
    let identity = database::pipeline::IconIdentity {
        code: code.to_string(),
        faction: parse_choice("faction", faction)?,
        category: parse_choice("category", category)?,
        crated,
        mod_name: mod_name.to_string(),
    };
    let resolutions = resolutions_arg(resolution)?;

    py.detach(|| {
        database::pipeline::modify_icon_in_database(
            &identity,
            icon.as_deref(),
            &resolutions,
            replace,
            remove,
            &db,
        )
    })
    .map_err(to_py_err)
}

/// Pack a directory into a .pak mod file.
///
/// Args:
///     input: Directory whose contents become the pak's file entries.
///     output: Path to write the .pak to.
///     mount_point: Mount point written into the pak header.
///     version: Pak version: one of "v8a", "v8b", "v11".
///     progress: Optional callable invoked as `progress(stage, done, total)`
///         with stage "packing", over the packed files.
///
/// Returns:
///     List of packed file paths (relative to `input`).
#[pyfunction]
#[pyo3(signature = (input, output, *, mount_point="../../../", version="v8b", progress=None))]
fn pack(
    py: Python<'_>,
    input: PathBuf,
    output: PathBuf,
    mount_point: &str,
    version: &str,
    progress: Option<Py<PyAny>>,
) -> PyResult<Vec<String>> {
    let version: crate::pak::PakVersion = parse_choice("pak version", version)?;
    let progress = PyProgress::new(progress);
    let result = py.detach(|| {
        crate::pak::pack_with_progress(
            &input,
            &output,
            mount_point,
            version.into(),
            &|done, total| progress.report("packing", done, total),
        )
    });
    progress.finish()?;
    result.map_err(to_py_err)
}

/// Decode a `.locres` file into an editable JSON translation file (see the
/// README's translation workflow).
///
/// Args:
///     input: Path to the `.locres` file to dump.
///     output: Path to write the JSON translation file to.
///     reference: Optional second `.locres` (typically `en`, the structural
///         superset) whose (namespace, key) set becomes the output's key set.
///
/// Returns:
///     Dict of build statistics (namespaces: namespace count).
#[pyfunction]
#[pyo3(signature = (input, output, *, reference=None))]
fn locres_dump(
    py: Python<'_>,
    input: PathBuf,
    output: PathBuf,
    reference: Option<PathBuf>,
) -> PyResult<Py<PyAny>> {
    let namespaces = py
        .detach(|| locres::dump_to_file(&input, reference.as_deref(), &output))
        .map_err(to_py_err)?;

    let stats_json = serde_json::json!({ "namespaces": namespaces });
    json_to_py(py, &stats_json)
}

/// Re-encode an edited JSON translation file (from `locres_dump`) into a
/// `.locres`.
///
/// Args:
///     input: Path to the edited JSON translation file.
///     output: Path to write the `.locres` to.
#[pyfunction]
fn locres_pack(py: Python<'_>, input: PathBuf, output: PathBuf) -> PyResult<()> {
    py.detach(|| locres::pack_file(&input, &output))
        .map_err(to_py_err)
}

/// Search `.uasset`/`.uexp` files under a directory for a literal string,
/// checking both ASCII and UTF-16LE encodings — for finding UI text baked
/// directly into a Blueprint/Widget asset instead of routed through a
/// `.locres` key.
///
/// Args:
///     pattern: Literal text to search for.
///     content_dir: Directory to recursively search.
///     progress: Optional callable invoked as `progress(stage, done, total)`
///         with stage "searching", over the searched files.
///
/// Returns:
///     List of dicts with `path` and `encoding` ("ascii" or "utf16le").
#[pyfunction]
#[pyo3(signature = (pattern, content_dir, *, progress=None))]
fn grep_strings(
    py: Python<'_>,
    pattern: &str,
    content_dir: PathBuf,
    progress: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    let progress = PyProgress::new(progress);
    let result = py.detach(|| {
        grep::search_content_dir_with_progress(&content_dir, pattern, &|done, total| {
            progress.report("searching", done, total)
        })
    });
    progress.finish()?;
    let matches = result.map_err(to_py_err)?;
    let matches_json: Vec<serde_json::Value> = matches
        .iter()
        .map(|m| {
            serde_json::json!({
                "path": m.path.to_string_lossy(),
                "encoding": m.encoding.to_string(),
            })
        })
        .collect();
    json_to_py(py, &serde_json::Value::Array(matches_json))
}

/// Write the icon of every catalog item a source provides as
/// `<CodeName>.png`, with the item's subtype icon drawn over its top-left
/// corner as in game unless `subicons` is false.
///
/// Args:
///     extract_dir: Directory a pak was already extracted into. Mutually
///         exclusive with `pak`; one of the two is required.
///     catalog: Existing catalog JSON listing the items whose icons are
///         extracted.
///     output: Directory the PNGs are written into.
///     pak: A `.pak` path, or a list of them in override order (later ones
///         win), read directly.
///     subicons: Whether to draw each item's subtype icon over its icon.
///     vanilla_extract_dir: Extraction directory of the base game's own pak,
///         used for the subtype icons the source doesn't itself provide.
///     vanilla_pak: The base game's `.pak`, read directly, as the same
///         fallback as `vanilla_extract_dir`.
///     progress: Optional callable invoked as `progress(stage, done, total)`
///         with stage "extracting_icons", "extracting_vanilla_icons" or
///         "writing_icons".
///
/// Returns:
///     Dict with `written`, `icons_missing`, `decode_errors` and
///     `subicons_missing` (icons written without the subtype icon they have,
///     because it was missing or failed to decode).
#[pyfunction]
#[pyo3(signature = (
    extract_dir=None, *, catalog, output, pak=None, subicons=true,
    vanilla_extract_dir=None, vanilla_pak=None, progress=None,
))]
#[allow(clippy::too_many_arguments)]
fn extract_icons(
    py: Python<'_>,
    extract_dir: Option<PathBuf>,
    catalog: PathBuf,
    output: PathBuf,
    pak: Option<PakPaths>,
    subicons: bool,
    vanilla_extract_dir: Option<PathBuf>,
    vanilla_pak: Option<PathBuf>,
    progress: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    if !subicons && (vanilla_extract_dir.is_some() || vanilla_pak.is_some()) {
        return Err(PyValueError::new_err(
            "a vanilla source is only used for subicons, which are turned off",
        ));
    }
    let paks = pak.map(PakPaths::into_paths).unwrap_or_default();
    let source = required_asset_source("extract_dir", extract_dir, "pak", paks)?;
    let request = ExtractIconsRequest {
        source,
        vanilla: asset_source(
            "vanilla_extract_dir",
            vanilla_extract_dir,
            "vanilla_pak",
            vanilla_pak.into_iter().collect(),
        )?,
        catalog,
        output,
        subicons,
    };
    let progress = PyProgress::new(progress);
    let result = py.detach(|| {
        crate::add_mod::extract_icons(&request, &|update: Progress| {
            progress.report(update.stage.as_str(), update.done, update.total)
        })
    });
    progress.finish()?;
    let stats = result.map_err(to_py_err)?;
    let result = PyDict::new(py);
    result.set_item("written", stats.written)?;
    result.set_item("icons_missing", stats.icons_missing)?;
    result.set_item("decode_errors", stats.decode_errors)?;
    result.set_item("subicons_missing", stats.subicons_missing)?;
    Ok(result.into_any().unbind())
}

/// Report how many of a catalog's icons some paks contain, reading only the
/// pak indexes (nothing is extracted).
///
/// Args:
///     catalog: Existing catalog JSON listing the items whose icons are
///         looked for.
///     pak: A `.pak` path, or a list of them.
///
/// Returns:
///     Dict with `main_icons_found`/`main_icons_total` (the items' own
///     icons), `overlays_found`/`overlays_total` (subtype icons and the
///     shared crate icon) and `has_crate_icon`. A mod needs main icons; when
///     it lacks overlays, the base game's pak supplies them as the vanilla
///     fallback of `add_mod`.
#[pyfunction]
#[pyo3(signature = (*, catalog, pak))]
fn pak_coverage(py: Python<'_>, catalog: PathBuf, pak: PakPaths) -> PyResult<Py<PyAny>> {
    let paks = pak.into_paths();
    if paks.is_empty() {
        return Err(PyValueError::new_err("at least one pak is required"));
    }
    let coverage = py
        .detach(|| crate::add_mod::pak_coverage(&catalog, &paks))
        .map_err(to_py_err)?;
    let result = PyDict::new(py);
    result.set_item("main_icons_found", coverage.main_icons_found)?;
    result.set_item("main_icons_total", coverage.main_icons_total)?;
    result.set_item("overlays_found", coverage.overlays_found)?;
    result.set_item("overlays_total", coverage.overlays_total)?;
    result.set_item("has_crate_icon", coverage.has_crate_icon)?;
    Ok(result.into_any().unbind())
}

/// Read the icon templates stored for one resolution of a database.
///
/// Args:
///     db: Path to the icon-template HDF5 database.
///     resolution: Vertical pixel height; must be a supported resolution.
///
/// Returns:
///     List of dicts, one per template, in storage order, with `code`,
///     `mod_name`, `faction` (`neutral`/`colonials`/`wardens`), `category`
///     (`item`/`vehicle`/`shippable`/`invalid`), `crated`, `phash`, `size`
///     (side in pixels) and `image` (`size * size * 3` row-major BGR bytes).
#[pyfunction]
fn read_templates(py: Python<'_>, db: PathBuf, resolution: i32) -> PyResult<Py<PyList>> {
    if !database::SUPPORTED_RESOLUTIONS.contains(&resolution) {
        return Err(PyValueError::new_err(format!(
            "unsupported resolution {resolution}"
        )));
    }
    let templates = py
        .detach(|| database::hdf5_writer::read_resolution(&db, resolution))
        .map_err(to_py_err)?;
    let size = database::icon_size_for_resolution(resolution);
    let list = PyList::empty(py);
    for template in &templates {
        let entry = PyDict::new(py);
        entry.set_item("code", &template.code)?;
        entry.set_item("mod_name", &template.mod_name)?;
        entry.set_item("faction", template.faction.name())?;
        entry.set_item("category", template.category.name())?;
        entry.set_item("crated", template.crated)?;
        entry.set_item("phash", template.phash)?;
        entry.set_item("size", size)?;
        entry.set_item("image", PyBytes::new(py, &template.image))?;
        list.append(entry)?;
    }
    Ok(list.unbind())
}

/// Count the templates of a database per mod and resolution.
///
/// Args:
///     db: Path to the icon-template HDF5 database.
///
/// Returns:
///     Dict with `resolutions` (the resolutions the database has, ascending)
///     and `mods` (mod name -> {resolution -> template count}; resolutions
///     where the mod has no template are absent).
#[pyfunction]
fn database_statistics(py: Python<'_>, db: PathBuf) -> PyResult<Py<PyAny>> {
    let stats = py
        .detach(|| database::hdf5_writer::read_statistics(&db))
        .map_err(to_py_err)?;
    let result = PyDict::new(py);
    result.set_item("resolutions", &stats.resolutions)?;
    let mods = PyDict::new(py);
    for (mod_name, counts) in &stats.per_mod {
        mods.set_item(mod_name, counts.clone())?;
    }
    result.set_item("mods", mods)?;
    Ok(result.into_any().unbind())
}

/// Get the crate version.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Run the `fs-assets` command-line interface.
///
/// Forwards `argv` (including the program name at index 0) to the exact
/// same clap parser used by the native binary, so the Python console
/// script accepts identical commands and parameters.
///
/// Args:
///     argv: Argument vector, typically `sys.argv`. Arguments that aren't
///         valid UTF-8 (which `sys.argv` holds as surrogate escapes) reach
///         the parser as the original bytes, as they do the native binary.
///
/// Returns:
///     Process exit code: 0 on success, 1 on error, 130 when Ctrl-C
///     stopped the command, and clap's code for `--help`, `--version` (0)
///     and bad arguments (2).
#[pyfunction]
fn cli_main(py: Python<'_>, argv: Vec<std::ffi::OsString>) -> i32 {
    // With the GIL released nothing runs Python's signal handlers, so the
    // long commands run them at every progress report: Ctrl-C stops them
    // there and they remove their temporary files, as with the native
    // binary. The short ones return before Python raises `KeyboardInterrupt`.
    py.detach(|| {
        crate::cli::run_with_interrupt_check(argv, &|| {
            Python::attach(|py| py.check_signals()).map_err(|_| crate::cli::Interrupted.into())
        })
    })
}

/// Native extension behind the `fs_assets` Python package.
#[pymodule]
fn _fs_assets(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(unpak, m)?)?;
    m.add_function(wrap_pyfunction!(convert_to_json, m)?)?;
    m.add_function(wrap_pyfunction!(convert_to_png, m)?)?;
    m.add_function(wrap_pyfunction!(build_catalog, m)?)?;
    m.add_function(wrap_pyfunction!(add_mod, m)?)?;
    m.add_function(wrap_pyfunction!(modify_icon, m)?)?;
    m.add_function(wrap_pyfunction!(extract_icons, m)?)?;
    m.add_function(wrap_pyfunction!(pak_coverage, m)?)?;
    m.add_function(wrap_pyfunction!(read_templates, m)?)?;
    m.add_function(wrap_pyfunction!(database_statistics, m)?)?;
    m.add_function(wrap_pyfunction!(pack, m)?)?;
    m.add_function(wrap_pyfunction!(locres_dump, m)?)?;
    m.add_function(wrap_pyfunction!(locres_pack, m)?)?;
    m.add_function(wrap_pyfunction!(grep_strings, m)?)?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(cli_main, m)?)?;
    // Fails when the embedding process already installed a logger.
    if log::set_logger(&PythonLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    Ok(())
}
