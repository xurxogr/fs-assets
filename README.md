# fs-assets

Tools for [Foxhole](https://www.foxholegame.com/) game assets, written in Rust
with Python bindings. It reads the game's `.pak` directly (no external
extraction or conversion tools) to:

- build the **item catalog**: every item, vehicle and structure that can be
  stored in a stockpile, with names in every game language, faction, icons and
  whether it can be crated;
- build the **icon-template database** (HDF5) used to recognise stockpile
  icons in screenshots, for the base game and for icon mods;
- dump and re-pack **`.locres` translations** and package them as a mod
  `.pak`.

It ships as a Python package (`fs_assets`) and a standalone command-line tool
(`fs-assets`) with the same operations.

## Installation

```bash
pip install fs-assets
```

Wheels are published for Linux (x86_64), Windows (x64) and macOS (arm64) with
HDF5 statically linked, so nothing else needs to be installed. A standalone
`fs-assets` binary for each platform is attached to every
[GitHub release](https://github.com/xurxogr/fs-assets/releases).

## Command line

The game pak is usually at
`<Steam>/steamapps/common/Foxhole/War/Content/Paks/War-WindowsNoEditor.pak`.

```bash
# Item catalog straight from the game pak
fs-assets build-catalog --pak War-WindowsNoEditor.pak -o catalog.json

# Icon-template database for the base game
fs-assets add-mod --pak War-WindowsNoEditor.pak --catalog catalog.json --db templates.h5

# Check an icon mod, then add it under its own name. Mods that don't ship the
# subtype/crate overlays borrow them from the game pak.
fs-assets check-paks --catalog catalog.json --pak IconMod.pak
fs-assets add-mod --pak IconMod.pak --vanilla-pak War-WindowsNoEditor.pak \
    --catalog catalog.json --db templates.h5 --mod-name icon-mod

# Every catalog icon as <CodeName>.png, with or without the subtype icons
fs-assets extract-icons --pak War-WindowsNoEditor.pak --catalog catalog.json -o icons
fs-assets extract-icons --pak War-WindowsNoEditor.pak --catalog catalog.json -o icons --no-subicons

# Replace one icon at two resolutions from a PNG
fs-assets modify-icon --db templates.h5 --code RifleW --faction wardens \
    --category item --icon rifle.png --resolution 1080,1440 --replace
```

| Command | What it does |
|---|---|
| `unpak` | Extract files from a `.pak` (optionally only paths containing `--include`). |
| `convert` | Convert one `.uasset` to JSON or PNG. |
| `build-catalog` | Build the catalog from the game pak or an extracted directory. `--full` keeps audio, visual and engine-internal fields. |
| `add-mod` | Add a mod's (or the base game's) icons to the template database. Re-adding a mod replaces its templates. `--pak` can be repeated (later paks win); `--resolution` rebuilds only some resolutions. |
| `check-paks` | Report how many of the catalog's icons and overlays some paks contain, reading only the pak indexes. |
| `extract-icons` | Write every catalog item's icon as `<CodeName>.png`, with its subtype icon drawn over it as in game; `--no-subicons` writes the bare icons. `--vanilla-pak` supplies the subtype icons a mod lacks. |
| `modify-icon` | Add, replace (`--replace`) or remove (`--remove`) a single template from a PNG or `.uasset`. |
| `locres-dump` / `locres-pack` | Convert a `.locres` to editable JSON and back. |
| `pack` | Pack a directory into a mod `.pak`. |
| `grep-strings` | Find text baked into `.uasset`/`.uexp` files instead of a `.locres`. |

Run `fs-assets <command> --help` for every option.

### Translation workflow

```bash
fs-assets unpak War-WindowsNoEditor.pak --include Localization -o game
fs-assets locres-dump game/War/Content/Localization/Foxhole-Content/pt/Foxhole-Content.locres \
    --reference game/War/Content/Localization/Foxhole-Content/en/Foxhole-Content.locres \
    -o pt.json
# edit the "text" fields of pt.json
mkdir -p mod/War/Content/Localization/Foxhole-Content/pt
fs-assets locres-pack pt.json -o mod/War/Content/Localization/Foxhole-Content/pt/Foxhole-Content.locres
fs-assets pack mod -o Translation.pak
```

`--reference` uses the English file, which contains every key, so keys missing
from the target language appear in the JSON with the English text to
translate. Hashes are carried over unchanged.

## Python

```python
import fs_assets

stats = fs_assets.build_catalog(pak="War-WindowsNoEditor.pak", output="catalog.json")
fs_assets.add_mod(
    pak=["IconMod_1.pak", "IconMod_2.pak"],
    vanilla_pak="War-WindowsNoEditor.pak",
    catalog="catalog.json",
    db="templates.h5",
    mod_name="icon-mod",
    progress=lambda stage, done, total: print(stage, done, total),
)

coverage = fs_assets.pak_coverage(catalog="catalog.json", pak="IconMod.pak")
for template in fs_assets.read_templates("templates.h5", 1080):
    print(template["code"], template["mod_name"], template["crated"])
print(fs_assets.database_statistics("templates.h5"))
```

Long operations release the GIL and accept a `progress(stage, done, total)`
callable, so they can run in a GUI worker thread. The package is fully typed;
the shapes of the returned dictionaries are in `fs_assets.types`.

## Development

Requires a Rust toolchain and, for local builds, the system HDF5 library
(`libhdf5-dev` on Debian/Ubuntu).

```bash
cargo test                                  # unit tests
cargo clippy --features python --all-targets -- -D warnings
cargo fmt --check

pip install maturin
maturin develop --release                   # build and install the Python package
pytest tests/python
```

Build with `--features static-hdf5` to compile HDF5 from source and link it
statically (needs CMake and a C compiler), as the release workflow does.

Some tests need real game assets in `tests/fixtures/`. Those files are not
distributable, so the tests are ignored by default; with the fixtures in place,
run them with `cargo test -- --ignored`.

Releases are published by pushing a `vX.Y.Z` tag that matches the version in
`Cargo.toml`.

## License

MIT. Foxhole is a trademark of Siege Camp; this project is not affiliated with
or endorsed by Siege Camp.
