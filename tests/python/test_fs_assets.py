"""Tests for fs_assets Python bindings.

Mirrors the Rust test suite's approach (see e.g. `src/catalog/pipeline.rs`):
tests that need real game assets are skipped when `tests/fixtures` isn't
present locally, since those fixtures aren't checked into the repo.
"""

import importlib
import importlib.metadata
import json
import struct
import zlib
from pathlib import Path

import fs_assets
import pytest
from fs_assets import _fs_assets as native

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures"
requires_fixtures = pytest.mark.skipif(
    not FIXTURES_DIR.exists(), reason="tests/fixtures not present locally"
)


def test_version_returns_string():
    assert isinstance(fs_assets.version(), str)
    assert fs_assets.version()


def test_package_version_needs_no_installed_metadata(monkeypatch):
    """Frozen apps (e.g. PyInstaller) ship no package metadata; import must still work."""

    def missing(name):
        raise importlib.metadata.PackageNotFoundError(name)

    monkeypatch.setattr(importlib.metadata, "version", missing)

    reloaded = importlib.reload(fs_assets)

    assert reloaded.__version__ == reloaded.version()


def test_unpak_nonexistent_pak_raises():
    with pytest.raises(RuntimeError):
        fs_assets.unpak("nonexistent.pak", "/tmp/fs-assets-test-out")


def test_convert_to_json_nonexistent_raises():
    with pytest.raises(RuntimeError):
        fs_assets.convert_to_json("nonexistent.uasset")


def test_modify_icon_invalid_faction_raises_value_error():
    with pytest.raises(ValueError):
        fs_assets.modify_icon(code="T_Test", faction="bogus", category="item", remove=True)


def test_modify_icon_invalid_category_raises_value_error():
    with pytest.raises(ValueError):
        fs_assets.modify_icon(code="T_Test", faction="neutral", category="bogus", remove=True)


def test_modify_icon_requires_icon_unless_removing(tmp_path):
    db = tmp_path / "database.h5"
    with pytest.raises(RuntimeError):
        fs_assets.modify_icon(str(db), code="T_Test", faction="neutral", category="item")


RIFLE_ICON = "War/Content/Textures/UI/ItemIcons/T_Rifle"


def _catalog(tmp_path):
    catalog = tmp_path / "catalog.json"
    catalog.write_text(json.dumps([{"CodeName": "Rifle", "Icon": RIFLE_ICON}]))
    return catalog


def _icon_mod_pak(tmp_path):
    icon = tmp_path / "content" / f"{RIFLE_ICON}.uasset"
    icon.parent.mkdir(parents=True)
    icon.write_bytes(b"not a real texture")
    pak = tmp_path / "icons.pak"
    native.pack(str(tmp_path / "content"), str(pak))
    return pak


def test_add_mod_requires_a_catalog():
    with pytest.raises(TypeError):
        fs_assets.add_mod(pak="War.pak")


def test_add_mod_requires_a_source():
    with pytest.raises(ValueError):
        fs_assets.add_mod(catalog="catalog.json")


def test_add_mod_rejects_both_extract_dir_and_pak():
    with pytest.raises(ValueError):
        fs_assets.add_mod("extracted", pak="War.pak", catalog="catalog.json")


def test_add_mod_rejects_both_vanilla_sources():
    with pytest.raises(ValueError):
        fs_assets.add_mod(
            pak="War.pak", catalog="catalog.json", vanilla_extract_dir="v", vanilla_pak="v.pak"
        )


def test_add_mod_missing_catalog_raises(tmp_path):
    with pytest.raises(RuntimeError, match="build-catalog"):
        fs_assets.add_mod(
            pak=str(_icon_mod_pak(tmp_path)),
            catalog=str(tmp_path / "missing.json"),
            db=str(tmp_path / "database.h5"),
        )


def test_add_mod_missing_pak_raises(tmp_path):
    with pytest.raises(RuntimeError):
        fs_assets.add_mod(
            pak=str(tmp_path / "missing.pak"),
            catalog=str(_catalog(tmp_path)),
            db=str(tmp_path / "database.h5"),
        )


def test_add_mod_imports_an_icon_only_pak_without_touching_the_catalog(tmp_path):
    catalog = _catalog(tmp_path)
    catalog_before = catalog.read_bytes()
    db = tmp_path / "database.h5"
    reports = []

    stats = fs_assets.add_mod(
        pak=str(_icon_mod_pak(tmp_path)),
        catalog=str(catalog),
        db=str(db),
        mod_name="icons",
        progress=lambda stage, done, total: reports.append((stage, done, total)),
    )

    stages = list(dict.fromkeys(stage for stage, _, _ in reports))
    assert stages == ["extracting_icons", "building_templates"]
    assert reports[-1] == ("building_templates", 1, 1)
    assert stats["total_items"] == 1
    assert catalog.read_bytes() == catalog_before
    assert db.exists()


def test_add_mod_reraises_progress_callback_errors(tmp_path):
    def fail(stage, done, total):
        raise KeyError(stage)

    with pytest.raises(KeyError):
        fs_assets.add_mod(
            pak=str(_icon_mod_pak(tmp_path)),
            catalog=str(_catalog(tmp_path)),
            db=str(tmp_path / "database.h5"),
            progress=fail,
        )


def _catalog_inputs_pak(tmp_path):
    content = tmp_path / "catalog_content"
    for file in ("War/Content/Blueprints/Data/placeholder.txt", "War/Content/Localization/x.txt"):
        (content / file).parent.mkdir(parents=True, exist_ok=True)
        (content / file).write_bytes(b"")
    pak = tmp_path / "game.pak"
    native.pack(str(content), str(pak))
    return pak


def test_build_catalog_requires_a_source():
    with pytest.raises(ValueError):
        fs_assets.build_catalog()


def test_build_catalog_rejects_both_extract_dir_and_pak():
    with pytest.raises(ValueError):
        fs_assets.build_catalog("extracted", pak="War.pak")


def test_build_catalog_rejects_a_pak_without_catalog_inputs(tmp_path):
    with pytest.raises(RuntimeError, match="has no catalog inputs"):
        fs_assets.build_catalog(pak=str(_icon_mod_pak(tmp_path)), output=str(tmp_path / "c.json"))


def test_build_catalog_from_a_pak_reports_progress_per_stage(tmp_path):
    output = tmp_path / "catalog.json"
    reports = []

    stats = fs_assets.build_catalog(
        pak=str(_catalog_inputs_pak(tmp_path)),
        output=str(output),
        progress=lambda stage, done, total: reports.append((stage, done, total)),
    )

    stages = list(dict.fromkeys(stage for stage, _, _ in reports))
    assert stages == ["extracting_catalog_inputs", "building_catalog"]
    assert reports[-1] == ("building_catalog", 0, 0)
    assert stats["total_files"] == 0
    assert json.loads(output.read_text()) == []


def _png(path, width, height, rgba):
    """Write a solid-color RGBA PNG without any imaging library."""

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    row = b"\x00" + bytes(rgba) * width
    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(row * height))
        + chunk(b"IEND", b"")
    )
    return path


def test_package_exports_every_native_function():
    for name in fs_assets.__all__:
        assert callable(getattr(fs_assets, name)), name


def test_add_mod_accepts_several_paks(tmp_path):
    first = _icon_mod_pak(tmp_path / "a")
    second = _icon_mod_pak(tmp_path / "b")

    stats = fs_assets.add_mod(
        pak=[str(first), str(second)],
        catalog=str(_catalog(tmp_path)),
        db=str(tmp_path / "database.h5"),
    )

    assert stats["icons_missing"] == 0


def test_add_mod_rejects_an_unsupported_resolution(tmp_path):
    with pytest.raises(RuntimeError, match="unsupported resolution 1234"):
        fs_assets.add_mod(
            pak=str(_icon_mod_pak(tmp_path)),
            catalog=str(_catalog(tmp_path)),
            db=str(tmp_path / "database.h5"),
            resolution=[1234],
        )


def test_png_icon_round_trips_through_read_templates_and_statistics(tmp_path):
    db = tmp_path / "database.h5"
    icon = _png(tmp_path / "rifle.png", 64, 64, (200, 100, 50, 255))

    fs_assets.modify_icon(
        str(db),
        code="Rifle",
        faction="wardens",
        category="item",
        icon=str(icon),
        resolution=[1080, 1440],
    )

    templates = fs_assets.read_templates(str(db), 1080)
    assert len(templates) == 1
    template = templates[0]
    assert (template["code"], template["faction"], template["category"]) == (
        "Rifle",
        "wardens",
        "item",
    )
    assert template["size"] == 32
    assert len(template["image"]) == 32 * 32 * 3
    assert template["image"][:3] == bytes([50, 100, 200])
    assert fs_assets.read_templates(str(db), 720) == []

    stats = fs_assets.database_statistics(str(db))
    assert stats["mods"] == {"vanilla": {1080: 1, 1440: 1}}


def test_read_templates_rejects_an_unsupported_resolution(tmp_path):
    db = tmp_path / "database.h5"
    icon = _png(tmp_path / "rifle.png", 8, 8, (0, 0, 0, 255))
    fs_assets.modify_icon(str(db), code="Rifle", faction="neutral", category="item", icon=str(icon))

    with pytest.raises(RuntimeError, match="unsupported resolution"):
        fs_assets.read_templates(str(db), 1234)


def test_cli_main_restores_the_sigint_handler(tmp_path):
    import signal

    before = signal.getsignal(signal.SIGINT)
    assert fs_assets.cli_main(["fs-assets", "grep-strings", "x", str(tmp_path / "missing")]) == 1
    assert signal.getsignal(signal.SIGINT) is before


def test_path_arguments_accept_pathlib_paths(tmp_path):
    db = tmp_path / "database.h5"
    icon = _png(tmp_path / "rifle.png", 64, 64, (200, 100, 50, 255))

    fs_assets.modify_icon(
        db, code="Rifle", faction="wardens", category="item", icon=icon, resolution=[1080]
    )

    assert len(fs_assets.read_templates(db, 1080)) == 1
    assert fs_assets.database_statistics(db)["mods"] == {"vanilla": {1080: 1}}
    coverage = fs_assets.pak_coverage(catalog=_catalog(tmp_path), pak=[_icon_mod_pak(tmp_path)])
    assert coverage["main_icons_found"] == 1


def test_unpak_extracts_what_pack_wrote(tmp_path):
    content = tmp_path / "content"
    (content / "War" / "Content").mkdir(parents=True)
    (content / "War" / "Content" / "a.uasset").write_bytes(b"first")
    (content / "War" / "Content" / "b.uexp").write_bytes(b"second")
    pak = tmp_path / "test.pak"
    fs_assets.pack(content, pak)

    out = tmp_path / "out"
    extracted = fs_assets.unpak(pak, out, include=".uasset")

    assert extracted == ["War/Content/a.uasset"]
    assert (out / "War" / "Content" / "a.uasset").read_bytes() == b"first"
    assert not (out / "War" / "Content" / "b.uexp").exists()


def test_locres_pack_and_dump_round_trip(tmp_path):
    document = {
        "version": 2,
        "namespaces": [
            {
                "namespace": "Items",
                "namespace_hash": 7,
                "entries": [
                    {"key": "Rifle", "key_hash": 1, "source_hash": 2, "text": "Fusil"},
                    {"key": "Mortar", "key_hash": 3, "source_hash": 4, "text": "Mortero ñ"},
                ],
            }
        ],
    }
    dump = tmp_path / "dump.json"
    dump.write_text(json.dumps(document), encoding="utf-8")
    locres = tmp_path / "Game.locres"

    fs_assets.locres_pack(dump, locres)
    stats = fs_assets.locres_dump(locres, tmp_path / "redump.json")

    assert stats["namespaces"] == 1
    redump = json.loads((tmp_path / "redump.json").read_text(encoding="utf-8"))
    for entry in document["namespaces"][0]["entries"]:
        entry["input_text"] = entry["text"]
    assert redump == document


def test_grep_strings_finds_ascii_and_utf16_matches(tmp_path):
    (tmp_path / "ascii.uasset").write_bytes(b"\x00\x01BP_Stockpile_C\x00")
    (tmp_path / "wide.uasset").write_bytes(b"\x00" + "Stockpile".encode("utf-16-le"))
    (tmp_path / "none.uasset").write_bytes(b"nothing here")

    matches = fs_assets.grep_strings("Stockpile", tmp_path)

    found = {(Path(m["path"]).name, m["encoding"]) for m in matches}
    assert found == {("ascii.uasset", "ascii"), ("wide.uasset", "utf16le")}


def test_pak_coverage_counts_catalog_icons(tmp_path):
    coverage = fs_assets.pak_coverage(
        catalog=str(_catalog(tmp_path)), pak=[str(_icon_mod_pak(tmp_path))]
    )

    assert coverage == {
        "main_icons_found": 1,
        "main_icons_total": 1,
        "overlays_found": 0,
        "overlays_total": 1,
        "has_crate_icon": False,
    }


@requires_fixtures
def test_build_catalog(tmp_path):
    output = tmp_path / "catalog.json"
    stats = fs_assets.build_catalog(str(FIXTURES_DIR), str(output))
    assert output.exists()
    assert stats["total_files"] == stats["parsed"]


@requires_fixtures
def test_add_mod(tmp_path):
    catalog = tmp_path / "catalog.json"
    db = tmp_path / "database.h5"
    fs_assets.build_catalog(str(FIXTURES_DIR), str(catalog))
    stats = fs_assets.add_mod(str(FIXTURES_DIR), catalog=str(catalog), db=str(db))
    assert db.exists()
    assert "items_embedded" in stats


def test_extract_icons_counts_undecodable_icons(tmp_path):
    stats = fs_assets.extract_icons(
        pak=str(_icon_mod_pak(tmp_path)),
        catalog=str(_catalog(tmp_path)),
        output=str(tmp_path / "icons"),
        subicons=False,
    )

    assert stats == {
        "written": 0,
        "icons_missing": 0,
        "decode_errors": 1,
        "subicons_missing": 0,
    }
