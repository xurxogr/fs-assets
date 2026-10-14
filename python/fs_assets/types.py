"""Types of the values the ``fs_assets`` functions take and return."""

from collections.abc import Callable
from typing import Literal, TypeAlias, TypedDict

#: Faction names, as ``modify_icon`` takes them and ``read_templates`` returns them.
Faction: TypeAlias = Literal["neutral", "colonials", "wardens"]
Category: TypeAlias = Literal["item", "vehicle", "shippable", "invalid"]
PakVersion: TypeAlias = Literal["v8a", "v8b", "v11"]
ProgressCallback: TypeAlias = Callable[[str, int, int], object]


class CatalogStats(TypedDict):
    total_files: int
    parsed: int
    stockpilable: int
    errors: int


class DatabaseBuildStats(TypedDict):
    total_items: int
    items_embedded: int
    missing_code: int
    icons_missing: int
    decode_errors: int


class ExtractIconsStats(TypedDict):
    written: int
    icons_missing: int
    decode_errors: int
    subicons_missing: int


class PakCoverage(TypedDict):
    main_icons_found: int
    main_icons_total: int
    overlays_found: int
    overlays_total: int
    has_crate_icon: bool


class Template(TypedDict):
    code: str
    mod_name: str
    faction: Faction
    category: Category
    crated: bool
    phash: int
    size: int
    image: bytes


class DatabaseStatistics(TypedDict):
    resolutions: list[int]
    mods: dict[str, dict[int, int]]


class LocresDumpStats(TypedDict):
    namespaces: int


class StringMatch(TypedDict):
    path: str
    encoding: Literal["ascii", "utf16le"]
