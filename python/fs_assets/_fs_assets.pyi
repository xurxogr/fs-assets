"""Type stubs for the native ``fs_assets._fs_assets`` extension."""

from collections.abc import Sequence
from typing import Any

from fs_assets.types import (
    CatalogStats,
    Category,
    DatabaseBuildStats,
    DatabaseStatistics,
    ExtractIconsStats,
    Faction,
    LocresDumpStats,
    PakCoverage,
    PakVersion,
    ProgressCallback,
    StringMatch,
    StrPath,
    Template,
)

__all__ = [
    "unpak",
    "convert_to_json",
    "convert_to_png",
    "build_catalog",
    "add_mod",
    "modify_icon",
    "extract_icons",
    "pak_coverage",
    "read_templates",
    "database_statistics",
    "pack",
    "locres_dump",
    "locres_pack",
    "grep_strings",
    "version",
    "cli_main",
]

def unpak(pak: StrPath, output: StrPath, *, include: str | None = None) -> list[str]: ...
def convert_to_json(path: StrPath) -> dict[str, Any]: ...
def convert_to_png(path: StrPath) -> bytes: ...
def build_catalog(
    extract_dir: StrPath | None = None,
    output: StrPath = "catalog.json",
    full: bool = False,
    *,
    pak: StrPath | None = None,
    progress: ProgressCallback | None = None,
) -> CatalogStats: ...
def add_mod(
    extract_dir: StrPath | None = None,
    *,
    catalog: StrPath,
    pak: StrPath | Sequence[StrPath] | None = None,
    db: StrPath = "database.h5",
    mod_name: str = "vanilla",
    vanilla_extract_dir: StrPath | None = None,
    vanilla_pak: StrPath | None = None,
    resolution: Sequence[int] | None = None,
    progress: ProgressCallback | None = None,
) -> DatabaseBuildStats: ...
def modify_icon(
    db: StrPath = "database.h5",
    *,
    code: str,
    faction: Faction,
    category: Category,
    crated: bool = False,
    mod_name: str = "vanilla",
    resolution: Sequence[int] | None = None,
    icon: StrPath | None = None,
    replace: bool = False,
    remove: bool = False,
) -> None: ...
def extract_icons(
    extract_dir: StrPath | None = None,
    *,
    catalog: StrPath,
    output: StrPath,
    pak: StrPath | Sequence[StrPath] | None = None,
    subicons: bool = True,
    vanilla_extract_dir: StrPath | None = None,
    vanilla_pak: StrPath | None = None,
) -> ExtractIconsStats: ...
def pak_coverage(*, catalog: StrPath, pak: StrPath | Sequence[StrPath]) -> PakCoverage: ...
def read_templates(db: StrPath, resolution: int) -> list[Template]: ...
def database_statistics(db: StrPath) -> DatabaseStatistics: ...
def pack(
    input: StrPath,
    output: StrPath,
    *,
    mount_point: str = "../../../",
    version: PakVersion = "v8b",
) -> list[str]: ...
def locres_dump(
    input: StrPath, output: StrPath, *, reference: StrPath | None = None
) -> LocresDumpStats: ...
def locres_pack(input: StrPath, output: StrPath) -> None: ...
def grep_strings(pattern: str, content_dir: StrPath) -> list[StringMatch]: ...
def version() -> str: ...
def cli_main(argv: list[str]) -> int: ...
