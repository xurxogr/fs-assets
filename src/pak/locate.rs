//! Finds the game's `.pak` directory in the local Steam install.

use std::fs;
use std::path::{Path, PathBuf};

/// The `.pak` directory, relative to a Steam library. Joined per component so
/// the result only has the platform's own separators.
const PAKS_IN_LIBRARY: [&str; 6] = ["steamapps", "common", "Foxhole", "War", "Content", "Paks"];

/// The directory holding the game's `.pak` files, or `None` when no Steam
/// library on this machine has them. From WSL, it also looks in the Steam
/// installed on the Windows side.
pub fn default_pak_directory() -> Option<PathBuf> {
    find_pak_directory(&steam_roots())
}

/// The first directory with `.pak` files among the libraries of `steam_roots`.
fn find_pak_directory(steam_roots: &[PathBuf]) -> Option<PathBuf> {
    let paks: PathBuf = PAKS_IN_LIBRARY.iter().collect();
    steam_roots
        .iter()
        .flat_map(|root| libraries(root))
        .map(|library| library.join(&paks))
        .find(|dir| contains_pak(dir))
}

/// A Steam install's own library, then the others it lists.
fn libraries(steam_root: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![steam_root.to_path_buf()];
    let listing = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(vdf) = fs::read_to_string(listing) {
        libraries.extend(library_paths(&vdf));
    }
    libraries
}

/// The `"path"` values of a `libraryfolders.vdf`.
fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| line.trim().strip_prefix("\"path\""))
        .filter_map(|value| value.trim().strip_prefix('"')?.strip_suffix('"'))
        .map(|value| native_path(&value.replace("\\\\", "\\")))
        .collect()
}

/// A library path as this process can open it: outside Windows, the Windows
/// paths of a Steam reached through WSL become their `/mnt/<drive>` mounts.
fn native_path(path: &str) -> PathBuf {
    if cfg!(windows) {
        return PathBuf::from(path);
    }
    match path.as_bytes() {
        [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic() => {
            let drive = char::from(drive.to_ascii_lowercase());
            let rest = path[3..].replace('\\', "/");
            Path::new("/mnt").join(drive.to_string()).join(rest)
        }
        _ => PathBuf::from(path),
    }
}

fn contains_pak(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pak"))
        })
    })
}

/// Where Steam says it is installed. The current user's value comes last: it
/// is written with forward slashes and in lowercase.
#[cfg(windows)]
fn steam_roots() -> Vec<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let read = |hive, key, value| -> Option<String> {
        RegKey::predef(hive)
            .open_subkey(key)
            .ok()?
            .get_value(value)
            .ok()
    };
    let machine = read(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\WOW6432Node\Valve\Steam",
        "InstallPath",
    );
    let user = read(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        .map(|path| path.replace('/', "\\"));
    machine.into_iter().chain(user).map(PathBuf::from).collect()
}

/// The usual Steam locations: native, Flatpak, Snap, then the Windows side
/// when running under WSL.
#[cfg(not(windows))]
fn steam_roots() -> Vec<PathBuf> {
    const IN_HOME: [&str; 4] = [
        ".steam/steam",
        ".local/share/Steam",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
        "snap/steam/common/.local/share/Steam",
    ];
    let home = std::env::home_dir();
    let native = home
        .iter()
        .flat_map(|home| IN_HOME.iter().map(move |root| home.join(root)));
    native
        .chain([PathBuf::from("/mnt/c/Program Files (x86)/Steam")])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_pak(library: &Path) -> PathBuf {
        let dir: PathBuf = PAKS_IN_LIBRARY.iter().collect();
        let dir = library.join(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("War-WindowsNoEditor.pak"), b"").unwrap();
        dir
    }

    #[test]
    fn finds_the_paks_in_the_steam_root_library() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = install_pak(tmp.path());
        assert_eq!(
            find_pak_directory(&[tmp.path().to_path_buf()]),
            Some(expected)
        );
    }

    #[test]
    fn finds_the_paks_in_a_library_the_root_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("steam");
        let library = tmp.path().join("other");
        let expected = install_pak(&library);
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::write(
            root.join("steamapps").join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                library.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        assert_eq!(find_pak_directory(&[root]), Some(expected));
    }

    #[test]
    fn skips_an_install_without_paks() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        let dir: PathBuf = PAKS_IN_LIBRARY.iter().collect();
        fs::create_dir_all(empty.join(dir)).unwrap();
        let full = tmp.path().join("full");
        let expected = install_pak(&full);

        assert_eq!(find_pak_directory(std::slice::from_ref(&empty)), None);
        assert_eq!(find_pak_directory(&[empty, full]), Some(expected));
    }

    #[test]
    fn no_steam_finds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(find_pak_directory(&[tmp.path().join("missing")]), None);
    }

    #[test]
    fn library_paths_reads_only_the_path_keys() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/games/steam\"\n\
                   \t\t\"label\"\t\t\"path\"\n\t}\n}\n";
        assert_eq!(library_paths(vdf), [PathBuf::from("/games/steam")]);
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_library_paths_become_wsl_mounts() {
        let vdf = "\t\t\"path\"\t\t\"D:\\\\Games\\\\Steam Library\"\n";
        assert_eq!(
            library_paths(vdf),
            [PathBuf::from("/mnt/d/Games/Steam Library")]
        );
    }
}
