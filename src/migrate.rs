//! LinuxPods' data and config directories, moved to Cymba's at startup:
//! docs/app/from-linuxpods.md.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The name each directory had up to 0.5.0.
pub const LINUXPODS: &str = "linuxpods";

/// What [`move_dir`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// What was at `from` is at `to`.
    Moved,
    /// No directory at `from`.
    NothingThere,
    /// `to` holds something already; `from` is left as it was.
    Kept,
}

/// The LinuxPods directory beside `dir`, one of Cymba's.
pub fn linuxpods_dir(dir: &Path) -> PathBuf {
    dir.with_file_name(LINUXPODS)
}

/// Moves the directory `from` to `to` in one rename, so its files keep their
/// modes and nothing is left half moved. Replaces nothing at `to` but an empty
/// directory: a directory with files in it, or anything else, is
/// [`Outcome::Kept`].
pub fn move_dir(from: &Path, to: &Path) -> std::io::Result<Outcome> {
    if !from.is_dir() {
        return Ok(Outcome::NothingThere);
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(Outcome::Moved),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists | ErrorKind::NotADirectory
            ) =>
        {
            Ok(Outcome::Kept)
        }
        Err(e) => Err(e),
    }
}

/// Moves LinuxPods' data and config directories to Cymba's, each where
/// [`move_dir`] can, and logs what it did. To run before anything reads them.
pub fn from_linuxpods() {
    for dir in [
        crate::keystore::default_dir(),
        crate::settings::default_dir(),
    ] {
        // The store that uses the directory fails the same way, and says so.
        let Ok(to) = dir else { continue };
        let from = linuxpods_dir(&to);
        match move_dir(&from, &to) {
            Ok(Outcome::Moved) => {
                tracing::info!("moved {} to {}", from.display(), to.display());
            }
            Ok(Outcome::NothingThere) => {}
            Ok(Outcome::Kept) => tracing::warn!(
                "{} is left as it is: {} exists and is not an empty directory",
                from.display(),
                to.display()
            ),
            Err(e) => tracing::error!("failed to move {} to {}: {e}", from.display(), to.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The files arrive with their contents and modes, and nothing is left
    /// under the old name.
    #[test]
    fn moves_the_files_with_their_modes() {
        let base = tempfile::tempdir().unwrap();
        let from = base.path().join(LINUXPODS);
        let to = base.path().join("cymba");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("keys.json"), "keys").unwrap();
        std::fs::set_permissions(from.join("keys.json"), PermissionsExt::from_mode(0o600)).unwrap();
        std::fs::write(from.join("readings.json"), "readings").unwrap();

        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Moved);

        assert_eq!(
            std::fs::read_to_string(to.join("keys.json")).unwrap(),
            "keys"
        );
        assert_eq!(mode(&to.join("keys.json")), 0o600);
        assert_eq!(
            std::fs::read_to_string(to.join("readings.json")).unwrap(),
            "readings"
        );
        assert!(!from.exists());
    }

    /// Cymba's own files win: neither directory changes.
    #[test]
    fn keeps_both_when_the_new_directory_has_files() {
        let base = tempfile::tempdir().unwrap();
        let from = base.path().join(LINUXPODS);
        let to = base.path().join("cymba");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        std::fs::write(from.join("keys.json"), "old").unwrap();
        std::fs::write(to.join("keys.json"), "new").unwrap();

        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Kept);

        assert_eq!(
            std::fs::read_to_string(from.join("keys.json")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(to.join("keys.json")).unwrap(),
            "new"
        );
    }

    /// An empty directory under the new name holds nothing to lose.
    #[test]
    fn replaces_an_empty_new_directory() {
        let base = tempfile::tempdir().unwrap();
        let from = base.path().join(LINUXPODS);
        let to = base.path().join("cymba");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        std::fs::write(from.join("settings.json"), "settings").unwrap();

        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Moved);

        assert_eq!(
            std::fs::read_to_string(to.join("settings.json")).unwrap(),
            "settings"
        );
        assert!(!from.exists());
    }

    /// A file under the new name is not replaced.
    #[test]
    fn keeps_a_file_under_the_new_name() {
        let base = tempfile::tempdir().unwrap();
        let from = base.path().join(LINUXPODS);
        let to = base.path().join("cymba");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(&to, "a file").unwrap();

        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Kept);

        assert!(from.is_dir());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "a file");
    }

    /// With nothing to move, nothing is made.
    #[test]
    fn makes_nothing_without_an_old_directory() {
        let base = tempfile::tempdir().unwrap();
        let to = base.path().join("cymba");

        assert_eq!(
            move_dir(&base.path().join(LINUXPODS), &to).unwrap(),
            Outcome::NothingThere
        );

        assert!(!to.exists());
    }
}
