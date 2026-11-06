//! The app's settings, kept across restarts: those in Preferences' General
//! group that do something.
//!
//! Stored at ~/.config/linuxpods/settings.json (XDG Base Directory):
//! {"version": 1, "battery_notifications": true, "pause_on_ear": true}
//!
//! Every field has a default, and a missing field takes it, so a file from an
//! older build loads. A field this build does not know is kept as it was read
//! and written back with the rest, and so is a newer `version`, so a file from
//! a newer build loads too, and its settings survive a change made here. A
//! file that does not parse is set aside and the defaults are used. There is
//! no file until a setting changes.
//!
//! A file, not GSettings: a GSettings schema has to be compiled and installed
//! wherever the app runs - `scripts/install.sh`, a release tarball - and GIO
//! aborts the process when it is missing, as it would be for a plain
//! `cargo run`.
//!
//! Loaded once at startup, before anything that uses a setting is set up;
//! [`Saver`] writes each change off the GTK main loop, in order.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const APP_NAME: &str = "linuxpods";
/// The settings file's name, inside [`default_dir`] or the directory given to
/// [`Store::with_dir`].
pub const SETTINGS_FILE: &str = "settings.json";
const FORMAT_VERSION: u32 = 1;

/// The settings, as the file holds them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The format the file was written in. Nothing depends on it yet: a field
    /// added later takes its default in an older file. A save never lowers
    /// it, so a newer build reading its file back sees its own version.
    version: u32,
    /// Whether low battery notifications are sent. On by default.
    pub battery_notifications: bool,
    /// Whether the media pauses when a pod leaves an ear, and plays again
    /// once it is back - see `crate::media`. On by default, as on an iPhone.
    pub pause_on_ear: bool,
    /// The fields this build does not know, from a newer build's file: kept
    /// and saved again as they were, so a change made here does not drop
    /// that build's settings.
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            battery_notifications: true,
            pause_on_ear: true,
            extra: Map::new(),
        }
    }
}

/// Reads and writes the settings file. Holds no settings of its own.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store in [`default_dir`]. Nothing is created until the first save.
    pub fn new() -> Result<Self> {
        Ok(Self {
            dir: default_dir()?,
        })
    }

    /// Test hook: use an explicit directory instead of the XDG one. Nothing is
    /// created until the first save.
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The settings file's path.
    pub fn path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Reads the settings. Never fails: whatever cannot be read leaves the
    /// defaults, with a warning.
    ///
    /// A missing file means nothing was ever changed. One that does not parse
    /// is renamed to `settings.json.corrupt-<unix seconds>` - kept, to be
    /// looked at, never deleted - and the next change starts a fresh file. One
    /// that cannot be read, or set aside, stays where it is, and the next
    /// change replaces it: a setting is the user's to set again, unlike a key.
    pub fn load(&self) -> Settings {
        let path = self.path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                tracing::debug!("no settings file at {}; using the defaults", path.display());
                return Settings::default();
            }
            Err(e) => {
                tracing::warn!(
                    "failed to read settings file {}: {e}; using the defaults",
                    path.display()
                );
                return Settings::default();
            }
        };
        match serde_json::from_slice::<Settings>(&raw) {
            Ok(settings) => {
                tracing::info!("settings loaded from {}", path.display());
                tracing::debug!("settings: {settings:?}");
                settings
            }
            Err(e) => {
                match self.set_aside() {
                    Ok(aside) => tracing::warn!(
                        "settings file {} did not parse ({e}); moved it to {} and using \
                         the defaults",
                        path.display(),
                        aside.display()
                    ),
                    Err(set_aside) => tracing::warn!(
                        "settings file {} did not parse ({e}), nor could it be moved aside \
                         ({set_aside:#}); using the defaults, and the next change replaces it",
                        path.display()
                    ),
                }
                Settings::default()
            }
        }
    }

    /// Renames a settings file that did not parse to
    /// `settings.json.corrupt-<unix seconds>`, returning the new path. It is
    /// never renamed over an earlier one.
    fn set_aside(&self) -> Result<PathBuf> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let aside = self.dir.join(format!("{SETTINGS_FILE}.corrupt-{secs}"));
        anyhow::ensure!(!aside.exists(), "{} already exists", aside.display());
        let path = self.path();
        std::fs::rename(&path, &aside)
            .with_context(|| format!("failed to move {} aside", path.display()))?;
        Ok(aside)
    }

    /// Replaces the settings file with `settings`, creating its directory if
    /// need be.
    ///
    /// They go to a temporary file that is then renamed over the old one, so a
    /// crash part way leaves the previous file whole rather than truncated.
    /// Both are created 0644 at most - never writable by anyone else - and the
    /// directory 0755.
    pub fn save(&self, settings: &Settings) -> Result<()> {
        let data = Settings {
            version: settings.version.max(FORMAT_VERSION),
            ..settings.clone()
        };
        let json =
            serde_json::to_vec_pretty(&data).context("failed to marshal settings to JSON")?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&self.dir)
            .with_context(|| format!("failed to create {}", self.dir.display()))?;

        // Named per process, so a second instance saving at the same moment
        // cannot write into this one's file.
        let tmp = self
            .dir
            .join(format!("{SETTINGS_FILE}.tmp-{}", std::process::id()));
        let path = self.path();
        let saved = write_file(&tmp, &json)
            .with_context(|| format!("failed to write {}", tmp.display()))
            .and_then(|()| {
                std::fs::rename(&tmp, &path)
                    .with_context(|| format!("failed to replace {}", path.display()))
            });
        if saved.is_err() {
            // Best effort: the next save replaces it anyway.
            let _ = std::fs::remove_file(&tmp);
        }
        saved
    }
}

/// Writes `data` to `path`, created 0644 if new, and through to the disk.
fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)?;
    file.write_all(data)?;
    // Before the rename, or a crash could leave the new name on data that never
    // reached the disk.
    file.sync_all()
}

/// Where [`Store::new`] keeps the settings: $XDG_CONFIG_HOME/linuxpods,
/// falling back to ~/.config/linuxpods.
pub fn default_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("failed to determine XDG config directory")?;
    Ok(base.join(APP_NAME))
}

/// The settings in use, and the writer that keeps the file up with them.
///
/// [`Saver::change`] changes them at once, in memory, and saves them on the
/// tokio runtime's blocking pool, so the GTK main loop never waits on the
/// disk. The saves land in order: a change records the settings it made as
/// the ones to write, and each save, once it holds the write lock, writes
/// whatever is recorded then - the latest - or nothing, if a save before it
/// already wrote that. However the blocking pool orders the saves, the last to
/// write writes the latest settings, so two quick changes never leave the
/// older on disk.
///
/// A save under way when the app quits still lands: dropping the runtime
/// waits for its blocking pool.
///
/// A save that fails is logged and not retried. The settings stay as changed,
/// in memory, until the app quits, and the next change writes them all.
#[derive(Clone)]
pub struct Saver {
    /// `None` when there is nowhere to save: the settings change in memory only.
    store: Option<Store>,
    runtime: tokio::runtime::Handle,
    shared: Arc<Shared>,
}

struct Shared {
    /// The settings in use, and whether they have changed since last written.
    /// Never held across a write: the main thread takes it on every change.
    slot: Mutex<Slot>,
    /// Held across a write, so one save writes at a time.
    writing: Mutex<()>,
}

struct Slot {
    current: Settings,
    unsaved: bool,
}

impl Saver {
    /// Starts with `settings`, as loaded from `store`; with no store, changes
    /// stay in memory.
    pub fn new(store: Option<Store>, settings: Settings, runtime: tokio::runtime::Handle) -> Self {
        Self {
            store,
            runtime,
            shared: Arc::new(Shared {
                slot: Mutex::new(Slot {
                    current: settings,
                    unsaved: false,
                }),
                writing: Mutex::new(()),
            }),
        }
    }

    /// The settings in use.
    pub fn get(&self) -> Settings {
        self.shared.slot().current.clone()
    }

    /// Changes the settings in use with `change`, and saves them in the
    /// background if that changed anything.
    pub fn change(&self, change: impl FnOnce(&mut Settings)) {
        {
            let mut slot = self.shared.slot();
            let before = slot.current.clone();
            change(&mut slot.current);
            if slot.current == before {
                return;
            }
            slot.unsaved = true;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let shared = self.shared.clone();
        self.runtime
            .spawn_blocking(move || shared.write_unsaved(&store));
    }
}

impl Shared {
    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the settings in use, if they have changed since last written.
    fn write_unsaved(&self, store: &Store) {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let settings = {
            let mut slot = self.slot();
            if !slot.unsaved {
                return;
            }
            slot.unsaved = false;
            slot.current.clone()
        };
        match store.save(&settings) {
            Ok(()) => tracing::info!("settings saved to {}", store.path().display()),
            Err(e) => tracing::warn!(
                "failed to save settings: {e:#}; the change holds until the app quits"
            ),
        }
    }
}

/// The settings for this run: loaded from [`Store::new`]'s file, and saved
/// there on every change.
pub fn open(runtime: tokio::runtime::Handle) -> Saver {
    let store = Store::new()
        .inspect_err(|e| tracing::warn!("settings will not be saved: {e:#}"))
        .ok();
    let settings = store.as_ref().map(Store::load).unwrap_or_default();
    Saver::new(store, settings, runtime)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::with_dir(dir.path().to_path_buf()), dir)
    }

    fn store_with(json: &str) -> (Store, tempfile::TempDir) {
        let (store, dir) = store();
        std::fs::write(dir.path().join(SETTINGS_FILE), json).unwrap();
        (store, dir)
    }

    fn off() -> Settings {
        Settings {
            battery_notifications: false,
            ..Settings::default()
        }
    }

    fn file_names(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(4)
            .build()
            .unwrap()
    }

    #[test]
    fn battery_notifications_are_on_by_default() {
        assert!(Settings::default().battery_notifications);
    }

    #[test]
    fn pausing_on_the_ears_is_on_by_default() {
        assert!(Settings::default().pause_on_ear);
    }

    /// A file without the setting turns it on, and one that turned it off
    /// keeps it off, with the settings it leaves out at their defaults.
    #[test]
    fn pausing_on_the_ears_loads_as_saved() {
        let (store, _dir) = store_with(r#"{"version":1,"battery_notifications":false}"#);
        assert!(store.load().pause_on_ear);
        let (store, _dir) = store_with(r#"{"version":1,"pause_on_ear":false}"#);
        let settings = store.load();
        assert!(!settings.pause_on_ear);
        assert!(settings.battery_notifications);
    }

    #[test]
    fn a_missing_file_means_the_defaults_and_writes_nothing() {
        let (store, dir) = store();
        assert_eq!(store.load(), Settings::default());
        assert!(file_names(&dir).is_empty());

        let gone = Store::with_dir(dir.path().join("not-yet"));
        assert_eq!(gone.load(), Settings::default());
        assert!(!dir.path().join("not-yet").exists());
    }

    #[test]
    fn a_saved_file_loads() {
        let (store, _dir) = store_with(r#"{"version":1,"battery_notifications":false}"#);
        assert_eq!(store.load(), off());
    }

    /// A file written before a setting existed gives it its default.
    #[test]
    fn a_missing_field_takes_its_default() {
        let (store, _dir) = store_with(r#"{"version":1}"#);
        assert_eq!(store.load(), Settings::default());
        let (store, _dir) = store_with("{}");
        assert_eq!(store.load(), Settings::default());
    }

    /// A file a newer build wrote loads all the same.
    #[test]
    fn an_unknown_field_does_not_stop_a_load() {
        let (store, dir) = store_with(
            r#"{"version":2,"battery_notifications":false,"auto_connect":true,"x":[1]}"#,
        );
        assert!(!store.load().battery_notifications);
        assert_eq!(file_names(&dir), [SETTINGS_FILE]);
    }

    /// A change made here keeps a newer build's fields and version: that
    /// build reads its own settings back.
    #[test]
    fn a_newer_files_fields_survive_a_save() {
        let (store, _dir) = store_with(
            r#"{"version":2,"battery_notifications":false,"auto_connect":true,"x":{"y":[1]}}"#,
        );
        let mut settings = store.load();
        settings.battery_notifications = true;
        store.save(&settings).unwrap();

        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": 2,
                "battery_notifications": true,
                "pause_on_ear": true,
                "auto_connect": true,
                "x": {"y": [1]},
            })
        );
        assert_eq!(store.load(), settings);
    }

    /// An older file is written back in this build's version.
    #[test]
    fn an_older_version_is_raised_on_save() {
        let (store, _dir) = store_with(r#"{"version":0,"battery_notifications":false}"#);
        store.save(&store.load()).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": FORMAT_VERSION,
                "battery_notifications": false,
                "pause_on_ear": true,
            })
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_set_aside() {
        // Arrays among them: derived without `flatten`, a struct would take
        // one as its fields in order.
        for bad in [
            "not json",
            "42",
            "[]",
            "[1,false]",
            r#"{"battery_notifications":"no"}"#,
        ] {
            let (store, dir) = store_with(bad);
            assert_eq!(store.load(), Settings::default(), "{bad}");

            let names = file_names(&dir);
            assert_eq!(names.len(), 1, "{names:?}");
            assert!(
                names[0].starts_with(&format!("{SETTINGS_FILE}.corrupt-")),
                "{names:?}"
            );
            let kept = std::fs::read_to_string(dir.path().join(&names[0])).unwrap();
            assert_eq!(kept, bad);

            // The next save starts a fresh file beside it.
            store.save(&off()).unwrap();
            assert_eq!(store.load(), off());
            assert_eq!(file_names(&dir).len(), 2);
        }
    }

    #[test]
    fn a_save_round_trips() {
        let (store, dir) = store();
        store.save(&off()).unwrap();
        assert_eq!(store.load(), off());
        store.save(&Settings::default()).unwrap();
        assert_eq!(store.load(), Settings::default());

        // No temporary file is left, and the file names its version.
        assert_eq!(file_names(&dir), [SETTINGS_FILE]);
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"version": 1, "battery_notifications": true, "pause_on_ear": true})
        );
    }

    #[test]
    fn a_save_creates_the_directory_and_no_one_else_can_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::with_dir(dir.path().join("config").join(APP_NAME));
        store.save(&off()).unwrap();
        assert_eq!(store.load(), off());

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(mode(&store.path()) & 0o022, 0);
        assert_eq!(mode(&store.dir) & 0o022, 0);
    }

    /// However the blocking pool orders two saves, the file ends with the
    /// later change: the save that runs first writes the latest settings,
    /// and the one after it finds nothing left to write.
    #[test]
    fn saves_run_out_of_order_still_leave_the_latest() {
        let (store, _dir) = store();
        let rt = runtime();
        let saver = Saver::new(
            Some(store.clone()),
            Settings::default(),
            rt.handle().clone(),
        );

        // Two changes whose saves have not run yet: done by hand here, as
        // `change` would leave them, with the saves run below in reverse.
        let mark = |settings: Settings| {
            let mut slot = saver.shared.slot();
            slot.current = settings;
            slot.unsaved = true;
        };
        mark(off());
        mark(Settings::default());
        saver.shared.write_unsaved(&store); // the second change's save
        assert_eq!(store.load(), Settings::default());
        std::fs::remove_file(store.path()).unwrap();
        saver.shared.write_unsaved(&store); // the first change's, run late
        assert!(!store.path().exists(), "the late save wrote again");
    }

    #[test]
    fn quick_changes_leave_the_last_on_disk() {
        let (store, _dir) = store();
        let rt = runtime();
        let saver = Saver::new(
            Some(store.clone()),
            Settings::default(),
            rt.handle().clone(),
        );
        for i in 0..101 {
            saver.change(|s| s.battery_notifications = i % 2 == 0);
        }
        assert!(saver.get().battery_notifications);
        // Dropping the runtime waits for the saves under way.
        drop(rt);
        assert!(store.load().battery_notifications);
        assert!(store.path().exists());
    }

    #[test]
    fn an_unchanged_setting_writes_nothing() {
        let (store, dir) = store();
        let rt = runtime();
        let saver = Saver::new(Some(store), Settings::default(), rt.handle().clone());
        saver.change(|s| s.battery_notifications = true);
        drop(rt);
        assert!(file_names(&dir).is_empty());
    }

    /// A save that fails leaves the change in use.
    #[test]
    fn a_failed_save_keeps_the_change() {
        let (_, dir) = store();
        // A file where the directory would be: nothing can be created in it.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let store = Store::with_dir(blocked.join(APP_NAME));

        let rt = runtime();
        let saver = Saver::new(
            Some(store.clone()),
            Settings::default(),
            rt.handle().clone(),
        );
        saver.change(|s| s.battery_notifications = false);
        drop(rt);
        assert!(store.save(&off()).is_err());
        assert_eq!(saver.get(), off());
    }

    #[test]
    fn without_a_store_changes_stay_in_memory() {
        let rt = runtime();
        let saver = Saver::new(None, Settings::default(), rt.handle().clone());
        saver.change(|s| s.battery_notifications = false);
        assert_eq!(saver.get(), off());
    }
}
