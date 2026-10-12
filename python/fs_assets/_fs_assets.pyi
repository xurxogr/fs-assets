"""Type stubs for the native ``fs_assets._fs_assets`` extension."""

from collections.abc import Sequence
from typing import Any

from fs_assets.types import (
    CatalogStats,
    Category,
    DatabaseBuildStats,
    DatabaseStatistics,
    Faction,
    LocresDumpStats,
    PakCoverage,
    PakVersion,
    ProgressCallback,
    StringMatch,
    Template,
)

__all__ = [
    "unpak",
    "convert_to_json",
    "convert_to_png",
    "build_catalog",
    "add_mod",
    "modify_icon",
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

def unpak(pak: str, output: str, *, include: str | None = None) -> list[str]: ...
def convert_to_json(path: str) -> dict[str, Any]: ...
def convert_to_png(path: str) -> bytes: ...
def build_catalog(
    extract_dir: str | None = None,
    output: str = "catalog.json",
    full: bool = False,
    *,
    pak: str | None = None,
    progress: ProgressCallback | None = None,
) -> CatalogStats: ...
def add_mod(
    extract_dir: str | None = None,
    *,
    catalog: str,
    pak: str | Sequence[str] | None = None,
    db: str = "database.h5",
    mod_name: str = "vanilla",
    vanilla_extract_dir: str | None = None,
    vanilla_pak: str | None = None,
    resolution: Sequence[int] | None = None,
    progress: ProgressCallback | None = None,
) -> DatabaseBuildStats: ...
def modify_icon(
    db: str = "database.h5",
    *,
    code: str,
    faction: Faction,
    category: Category,
    crated: bool = False,
    mod_name: str = "vanilla",
    resolution: Sequence[int] | None = None,
    icon: str | None = None,
    replace: bool = False,
    remove: bool = False,
) -> None: ...
def pak_coverage(*, catalog: str, pak: str | Sequence[str]) -> PakCoverage: ...
def read_templates(db: str, resolution: int) -> list[Template]: ...
def database_statistics(db: str) -> DatabaseStatistics: ...
def pack(
    input: str, output: str, *, mount_point: str = "../../../", version: PakVersion = "v8b"
) -> list[str]: ...
def locres_dump(input: str, output: str, *, reference: str | None = None) -> LocresDumpStats: ...
def locres_pack(input: str, output: str) -> None: ...
def grep_strings(pattern: str, content_dir: str) -> list[StringMatch]: ...
def version() -> str: ...
def cli_main(argv: list[str]) -> int: ...
