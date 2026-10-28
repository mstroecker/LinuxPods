//! Persistent storage for AirPods BLE encryption keys.
//!
//! Stored at ~/.local/share/linuxpods/keys.json (XDG Base Directory), mode 0600:
//! {"version": 1, "keys": {"MAC": "base64-key", ...}}
//!
//! MACs are written as [`Address`] displays them, in uppercase, and parsed on
//! load in whatever case they were written: a MAC used to be stored as it was
//! given, and `examples/key_request` takes it from its command line.
//!
//! Persistence only: the coordinator holds the keys and hands the whole map to
//! [`Keystore::save`]. A second copy kept here went stale against the
//! coordinator's, and a save that failed was then never retried.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bluer::Address;
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "linuxpods";
/// The key file's name, inside [`default_dir`] or the directory given to
/// [`Keystore::with_dir`].
pub const KEYS_FILE: &str = "keys.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct KeyData {
    version: u32,
    /// MAC address, as text -> base64-encoded key
    keys: HashMap<String, String>,
}

/// Reads and writes the key file. Holds no keys of its own.
#[derive(Clone)]
pub struct Keystore {
    data_dir: PathBuf,
}

impl Keystore {
    /// Creates the data directory (0700) if absent.
    pub fn new() -> Result<Self> {
        let data_dir = default_dir()?;
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;
        restrict_permissions(&data_dir, 0o700)?;
        Ok(Self { data_dir })
    }

    /// Test hook: use an explicit directory instead of the XDG one.
    pub fn with_dir(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        Ok(Self { data_dir })
    }

    fn path(&self) -> PathBuf {
        self.data_dir.join(KEYS_FILE)
    }

    /// Reads all keys from disk. A missing file is not an error - nothing has been
    /// stored yet - but anything else is, down to a single entry whose MAC does
    /// not parse or whose key does not decode. A file that did not load must not
    /// be saved over, or every key in it is lost: see [`Keystore::set_aside`].
    ///
    /// Two entries can spell one address differently, since a MAC used to be
    /// stored as it was given. Holding the same key they are one entry. Holding
    /// different keys, the one spelled as [`Keystore::save`] writes it wins,
    /// with a warning; the next save leaves only that one.
    pub fn load(&self) -> Result<HashMap<Address, Vec<u8>>> {
        let path = self.path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read keys file {}", path.display()));
            }
        };
        let parsed: KeyData = serde_json::from_slice(&raw).context("failed to parse keys JSON")?;

        let mut entries = Vec::with_capacity(parsed.keys.len());
        for (spelling, b64) in parsed.keys {
            let mac: Address = spelling
                .parse()
                .with_context(|| format!("invalid MAC address {spelling:?}"))?;
            let key = B64
                .decode(b64.as_bytes())
                .with_context(|| format!("failed to decode key for {spelling}"))?;
            let canonical = spelling == mac.to_string();
            entries.push((!canonical, spelling, mac, key));
        }
        // The canonical spelling first, so that it is the one kept; then by
        // spelling, so the choice does not hang on the order the entries come
        // out of the map in. Spellings are unique, so nothing after them counts.
        entries.sort_unstable();

        let mut keys = HashMap::with_capacity(entries.len());
        for (_, spelling, mac, key) in entries {
            match keys.entry(mac) {
                Entry::Vacant(slot) => {
                    slot.insert(key);
                }
                Entry::Occupied(kept) if *kept.get() == key => {}
                Entry::Occupied(_) => tracing::warn!(
                    "keys file holds two different keys for {mac}; ignoring the one \
                     stored as {spelling:?}"
                ),
            }
        }
        Ok(keys)
    }

    /// Renames a key file that did not load to `keys.json.corrupt-<unix seconds>`,
    /// returning the new path.
    ///
    /// The next save then starts a fresh file, and the keys in the old one stay
    /// recoverable by hand. It is never deleted, nor renamed over an earlier one.
    pub(crate) fn set_aside(&self) -> Result<PathBuf> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let aside = self.data_dir.join(format!("{KEYS_FILE}.corrupt-{secs}"));
        anyhow::ensure!(!aside.exists(), "{} already exists", aside.display());

        let path = self.path();
        std::fs::rename(&path, &aside)
            .with_context(|| format!("failed to move {} aside", path.display()))?;
        Ok(aside)
    }

    /// Replaces the key file with `keys`. Empty keys are left out: one decrypts
    /// nothing, and on disk it would come back on every start.
    ///
    /// The keys go to a temporary file that is then renamed over the old one, so
    /// a crash part way leaves the previous file whole rather than truncated.
    pub fn save(&self, keys: &HashMap<Address, Vec<u8>>) -> Result<()> {
        let data = KeyData {
            version: FORMAT_VERSION,
            keys: keys
                .iter()
                .filter(|(_, key)| !key.is_empty())
                .map(|(mac, key)| (mac.to_string(), B64.encode(key)))
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&data).context("failed to marshal keys to JSON")?;

        // Named per process, so a second instance saving at the same moment
        // cannot write into this one's file.
        let tmp = self
            .data_dir
            .join(format!("{KEYS_FILE}.tmp-{}", std::process::id()));
        let path = self.path();
        let saved = write_private(&tmp, &json)
            .with_context(|| format!("failed to write {}", tmp.display()))
            .and_then(|()| {
                std::fs::rename(&tmp, &path)
                    .with_context(|| format!("failed to replace keys file {}", path.display()))
            });
        if saved.is_err() {
            // It holds keys too. Best effort: the next save replaces it anyway.
            let _ = std::fs::remove_file(&tmp);
        }
        saved
    }
}

/// Creates `path` as a new file, 0600 from the start - never readable by anyone
/// else, even briefly - and writes `data` through to the disk.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // `mode` applies only to a file this call creates, so a leftover - from a
    // crashed process that had the same pid - goes first.
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    // Before the rename, or a crash could leave the new name on data that never
    // reached the disk.
    file.sync_all()
}

/// Where [`Keystore::new`] keeps the keys: $XDG_DATA_HOME/linuxpods, falling
/// back to ~/.local/share/linuxpods.
pub fn default_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("failed to determine XDG data directory")?;
    Ok(base.join(APP_NAME))
}

fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    fn store() -> (Keystore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Keystore::with_dir(dir.path().to_path_buf()).unwrap(), dir)
    }

    fn keys(key: &[u8]) -> HashMap<Address, Vec<u8>> {
        HashMap::from([(MAC, key.to_vec())])
    }

    /// A key file holding `entries`, MAC to base64 key, in that order.
    fn store_with(entries: &[(&str, &str)]) -> (Keystore, tempfile::TempDir) {
        let (ks, dir) = store();
        let entries: Vec<String> = entries
            .iter()
            .map(|(mac, key)| format!("{mac:?}:{key:?}"))
            .collect();
        let json = format!(r#"{{"version":1,"keys":{{{}}}}}"#, entries.join(","));
        std::fs::write(dir.path().join(KEYS_FILE), json).unwrap();
        (ks, dir)
    }

    /// Synthetic keys: sixteen 0x01 bytes, and sixteen 0x02.
    const ONES: &str = "AQEBAQEBAQEBAQEBAQEBAQ==";
    const TWOS: &str = "AgICAgICAgICAgICAgICAg==";

    fn file_names(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn missing_file_loads_empty() {
        let (ks, _d) = store();
        assert!(ks.load().unwrap().is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let (ks, dir) = store();
        let key = vec![1u8; 16];
        ks.save(&keys(&key)).unwrap();

        let reopened = Keystore::with_dir(dir.path().to_path_buf()).unwrap();
        let loaded = reopened.load().unwrap();
        assert_eq!(loaded.get(&MAC), Some(&key));
    }

    #[test]
    fn never_persists_an_empty_key() {
        let (ks, _d) = store();
        let mut map = keys(&[1u8; 16]);
        map.insert(
            Address::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]),
            Vec::new(),
        );
        ks.save(&map).unwrap();

        assert_eq!(ks.load().unwrap(), keys(&[1u8; 16]));
    }

    #[test]
    fn reads_go_written_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(KEYS_FILE),
            r#"{"version":1,"keys":{"AA:BB:CC:DD:EE:FF":"AQIDBAUGBwgJCgsMDQ4PEA=="}}"#,
        )
        .unwrap();

        let ks = Keystore::with_dir(dir.path().to_path_buf()).unwrap();
        let loaded = ks.load().unwrap();
        assert_eq!(loaded[&MAC], (1u8..=16).collect::<Vec<u8>>());
    }

    /// Regression (#47): a key stored under a lowercase MAC - as
    /// `examples/key_request` did with the MAC it was given - was keyed apart
    /// from the uppercase MAC BlueZ reports, and the pair showed up twice.
    #[test]
    fn a_lowercase_mac_loads_as_the_same_address() {
        let (ks, _d) = store_with(&[("aa:bb:cc:dd:ee:ff", ONES)]);
        assert_eq!(ks.load().unwrap(), keys(&[1u8; 16]));
    }

    /// Two spellings of one address holding the same key are one entry, and a
    /// save writes it once, as `Address` displays it.
    #[test]
    fn two_spellings_with_the_same_key_merge() {
        let (ks, dir) = store_with(&[("aa:bb:cc:dd:ee:ff", ONES), ("AA:BB:CC:DD:EE:FF", ONES)]);
        let loaded = ks.load().unwrap();
        assert_eq!(loaded, keys(&[1u8; 16]));

        ks.save(&loaded).unwrap();
        let saved = std::fs::read_to_string(dir.path().join(KEYS_FILE)).unwrap();
        assert!(saved.contains("\"AA:BB:CC:DD:EE:FF\""), "{saved}");
        assert!(!saved.contains("aa:bb"), "{saved}");
    }

    /// Two spellings holding different keys: the one spelled the way `save`
    /// writes it wins, wherever it sits in the file.
    #[test]
    fn of_two_different_keys_the_canonical_spelling_wins() {
        for entries in [
            [("aa:bb:cc:dd:ee:ff", TWOS), ("AA:BB:CC:DD:EE:FF", ONES)],
            [("AA:BB:CC:DD:EE:FF", ONES), ("aa:bb:cc:dd:ee:ff", TWOS)],
        ] {
            let (ks, _d) = store_with(&entries);
            assert_eq!(ks.load().unwrap(), keys(&[1u8; 16]));
        }
    }

    /// A MAC that does not parse fails the load, like a key that does not
    /// decode: the file is then set aside rather than saved over.
    #[test]
    fn an_unparsable_mac_fails_the_load() {
        let (ks, _d) = store_with(&[("AA:BB:CC:DD:EE:FF", ONES), ("not-a-mac", TWOS)]);
        let err = ks.load().unwrap_err();
        assert!(format!("{err:#}").contains("not-a-mac"), "{err:#}");
    }

    #[test]
    fn save_uses_owner_only_permissions() {
        let (ks, dir) = store();
        ks.save(&keys(&[9u8; 16])).unwrap();
        let mode = std::fs::metadata(dir.path().join(KEYS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The temporary file is renamed into place, not left beside it.
    #[test]
    fn save_leaves_only_the_key_file() {
        let (ks, dir) = store();
        ks.save(&keys(&[1u8; 16])).unwrap();
        ks.save(&keys(&[2u8; 16])).unwrap();

        assert_eq!(file_names(&dir), [KEYS_FILE]);
        assert_eq!(ks.load().unwrap(), keys(&[2u8; 16]));
    }

    /// Regression (#38): the file was truncated and then written, so a save that
    /// failed part way left it empty or cut short.
    #[test]
    fn a_failed_save_leaves_the_previous_file_whole() {
        let (ks, dir) = store();
        ks.save(&keys(&[1u8; 16])).unwrap();

        // Read-only, so the temporary file cannot be created.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = ks.save(&keys(&[2u8; 16]));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(failed.is_err());
        assert_eq!(ks.load().unwrap(), keys(&[1u8; 16]));
        assert_eq!(file_names(&dir), [KEYS_FILE]);
    }

    /// The file goes aside byte for byte, clearing the way for a fresh one.
    #[test]
    fn set_aside_keeps_a_file_that_did_not_load() {
        let (ks, dir) = store();
        let corrupt = r#"{"version":1,"keys":{"AA:BB:CC:DD:EE:FF":"not base64!"}}"#;
        std::fs::write(dir.path().join(KEYS_FILE), corrupt).unwrap();
        assert!(ks.load().is_err());

        let aside = ks.set_aside().unwrap();

        assert_eq!(aside.parent(), Some(dir.path()));
        let name = aside.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("keys.json.corrupt-"), "{name}");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), corrupt);
        assert!(ks.load().unwrap().is_empty(), "keys.json is gone");
    }
}
