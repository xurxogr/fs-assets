"""fs-assets: Foxhole game asset tools.

Reads the game's ``.pak`` directly (no external tools) to build the item
catalog and the icon-template database used for stockpile OCR, and to work
with ``.locres`` translations and mod paks.

Example:
    >>> import fs_assets
    >>> fs_assets.build_catalog(pak="War.pak", output="catalog.json")
    >>> fs_assets.add_mod(pak="War.pak", catalog="catalog.json", db="database.h5")
    >>> fs_assets.add_mod(
    ...     pak=["Mod_1.pak", "Mod_2.pak"], vanilla_pak="War.pak",
    ...     catalog="catalog.json", db="database.h5", mod_name="mod",
    ... )
"""

import sys
from importlib.metadata import version as _pkg_version

from ._fs_assets import (
    add_mod,
    build_catalog,
    cli_main,
    convert_to_json,
    convert_to_png,
    database_statistics,
    grep_strings,
    locres_dump,
    locres_pack,
    modify_icon,
    pack,
    pak_coverage,
    read_templates,
    unpak,
    version,
)

__all__ = [
    "add_mod",
    "build_catalog",
    "cli_main",
    "convert_to_json",
    "convert_to_png",
    "database_statistics",
    "grep_strings",
    "locres_dump",
    "locres_pack",
    "main",
    "modify_icon",
    "pack",
    "pak_coverage",
    "read_templates",
    "unpak",
    "version",
]

__version__ = _pkg_version("fs-assets")


def main() -> None:
    """Entry point for the ``fs-assets`` console script.

    Forwards ``sys.argv`` to the Rust CLI, so the command exposes the exact
    same subcommands and flags as the native binary.
    """
    raise SystemExit(cli_main(sys.argv))
