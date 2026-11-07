//! Each pair's last BLE reading, kept across a relaunch (#170): a pair put
//! away in its closed case says nothing until it is opened again, and its
//! last levels are still a hint of how charged it was.
//!
//! Stored at ~/.local/share/linuxpods/readings.json, beside the keys
//! ([`crate::keystore`]), mode 0600:
//! {"version": 1, "readings": {"MAC": {"left_battery": 80, "right_battery": 79,
//! "case_battery": null, "left_charging": false, "right_charging": false,
//! "case_charging": false, "product_id": 8231, "color": 0, "heard": 1793741098}}}
//!
//! `heard` is when the advertisement behind the reading arrived, in Unix
//! seconds: the wall clock, as the next run has to tell how old it is - see
//! [`podstate::age`]. What a reading holds is what still says something once
//! the app has been away: the levels, the charging flags and which model it
//! is. Not in-ear, the lid or what the pods were doing, which are stale by
//! the time anything loads them, nor the advertisement itself.
//!
//! Persistence only, like the keystore: the coordinator holds the readings,
//! loads them at startup - those of a pair with keys, heard less than
//! [`podstate::BLE_CACHE_TTL`] ago and not after now (see [`podstate::age`]) -
//! and hands the whole map to
//! [`Store::save`]. It saves no more often than its expiry clock ticks, and
//! only when [`due`] says so, never per advertisement - see
//! `docs/app/readings.md#saving`.
//!
//! A file that does not parse is set aside and the app starts with no
//! readings, which costs only what it would have shown until the pairs are
//! heard again. A level above 100 loads as unknown, as the parser reads one.
//!
//! A reading loaded here is shown, but not exported to GNOME Settings until
//! the pair is heard again this run: see `PodState::restored`.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bluer::Address;
use serde::{Deserialize, Serialize};

use crate::keystore;
use crate::podstate::{self, Identity};

/// The readings file's name, inside the directory given to
/// [`Store::with_dir`]: the keystore's.
pub const READINGS_FILE: &str = "readings.json";
const FORMAT_VERSION: u32 = 1;

/// How long a reading's heard time may run ahead of the one saved before a
/// save is due for it alone - see [`due`]. A choice: a pair in range has a
/// newer heard time at every save on the expiry clock (`expiry_task`, once a
/// minute), and saved at that pace the file would be written all day.
/// The cost is a relaunch after a crash or a power cut showing a reading up to
/// this much older than it was; the app quitting saves it as it is.
pub const HEARD_REFRESH: Duration = Duration::from_secs(10 * 60);

/// One pair's last BLE reading, as kept across a relaunch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub left_battery: Option<u8>,
    pub right_battery: Option<u8>,
    pub case_battery: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
    pub identity: Identity,
    /// When the advertisement behind it arrived, on the wall clock.
    pub heard: SystemTime,
}

#[derive(Serialize, Deserialize)]
struct ReadingsData {
    version: u32,
    /// MAC address, as [`Address`] displays it -> its reading.
    readings: HashMap<String, StoredReading>,
}

/// A [`Reading`] as the file holds it.
#[derive(Serialize, Deserialize)]
struct StoredReading {
    left_battery: Option<u8>,
    right_battery: Option<u8>,
    case_battery: Option<u8>,
    left_charging: bool,
    right_charging: bool,
    case_charging: bool,
    product_id: u16,
    color: u8,
    /// Unix seconds.
    heard: u64,
}

impl StoredReading {
    fn of(reading: &Reading) -> Self {
        Self {
            left_battery: reading.left_battery,
            right_battery: reading.right_battery,
            case_battery: reading.case_battery,
            left_charging: reading.left_charging,
            right_charging: reading.right_charging,
            case_charging: reading.case_charging,
            product_id: reading.identity.product_id,
            color: reading.identity.color,
            // A time before 1970 is no time a reading was heard at.
            heard: reading
                .heard
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    /// The reading, `None` for a heard time out of range. A level above 100
    /// is unknown, as the parser reads one: the file is only ours by its name.
    fn reading(&self) -> Option<Reading> {
        let level = |level: Option<u8>| level.filter(|&level| level <= 100);
        Some(Reading {
            left_battery: level(self.left_battery),
            right_battery: level(self.right_battery),
            case_battery: level(self.case_battery),
            left_charging: self.left_charging,
            right_charging: self.right_charging,
            case_charging: self.case_charging,
            identity: Identity {
                product_id: self.product_id,
                color: self.color,
            },
            heard: UNIX_EPOCH.checked_add(Duration::from_secs(self.heard))?,
        })
    }
}

/// Reads and writes the readings file. Holds no readings of its own.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store in `dir`, which the keystore has made: nothing is created
    /// here but the file, at the first save.
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The readings file's path.
    pub fn path(&self) -> PathBuf {
        self.dir.join(READINGS_FILE)
    }

    /// Reads every reading in the file, however old, and of whichever pair:
    /// which to keep is the coordinator's to judge. A missing file is not an
    /// error - nothing has been saved yet - but anything else is, down to a
    /// single entry whose MAC does not parse or whose heard time is out of
    /// range.
    pub fn load(&self) -> Result<HashMap<Address, Reading>> {
        let path = self.path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read readings file {}", path.display()));
            }
        };
        let parsed: ReadingsData =
            serde_json::from_slice(&raw).context("failed to parse readings JSON")?;
        let mut readings = HashMap::with_capacity(parsed.readings.len());
        for (spelling, stored) in parsed.readings {
            let mac: Address = spelling
                .parse()
                .with_context(|| format!("invalid MAC address {spelling:?}"))?;
            let reading = stored
                .reading()
                .with_context(|| format!("the reading of {spelling} was heard at no time"))?;
            readings.insert(mac, reading);
        }
        Ok(readings)
    }

    /// Renames a readings file that did not load to
    /// `readings.json.corrupt-<unix seconds>`, returning the new path. It is
    /// never renamed over an earlier one.
    pub fn set_aside(&self) -> Result<PathBuf> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let aside = self.dir.join(format!("{READINGS_FILE}.corrupt-{secs}"));
        anyhow::ensure!(!aside.exists(), "{} already exists", aside.display());
        let path = self.path();
        std::fs::rename(&path, &aside)
            .with_context(|| format!("failed to move {} aside", path.display()))?;
        Ok(aside)
    }

    /// Replaces the readings file with `readings`, in format version 1.
    ///
    /// They go to a temporary file, 0600 from the start, that is then renamed
    /// over the old one, so a crash part way leaves the previous file whole
    /// rather than truncated.
    pub fn save(&self, readings: &HashMap<Address, Reading>) -> Result<()> {
        let data = ReadingsData {
            version: FORMAT_VERSION,
            readings: readings
                .iter()
                .map(|(mac, reading)| (mac.to_string(), StoredReading::of(reading)))
                .collect(),
        };
        let json =
            serde_json::to_vec_pretty(&data).context("failed to marshal readings to JSON")?;

        // Named per process, so a second instance saving at the same moment
        // cannot write into this one's file.
        let tmp = self
            .dir
            .join(format!("{READINGS_FILE}.tmp-{}", std::process::id()));
        let path = self.path();
        let saved = keystore::write_private(&tmp, &json)
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

/// Whether `current` is worth writing over `saved`, the readings last
/// written: a pair has come or gone - an expired reading goes from the file
/// too - or a level, a charging flag or the model has changed, or a heard time
/// has moved by `refresh` or more. A heard time that went back, the clock set
/// back, has moved.
///
/// [`HEARD_REFRESH`] while the app runs, so a pair in range costs a write
/// every so often rather than every minute; `Duration::ZERO` as it quits, so
/// the file is left with every reading as it is.
pub fn due(
    saved: &HashMap<Address, Reading>,
    current: &HashMap<Address, Reading>,
    refresh: Duration,
) -> bool {
    saved.len() != current.len()
        || current.iter().any(|(mac, now)| {
            let Some(then) = saved.get(mac) else {
                return true;
            };
            let moved = now.heard != then.heard
                && podstate::age(then.heard, now.heard).is_none_or(|moved| moved >= refresh);
            Reading {
                heard: then.heard,
                ..*now
            } != *then
                || moved
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const MAC: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
    const OTHER: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0x02]);

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::with_dir(dir.path().to_path_buf()), dir)
    }

    /// A reading heard at `heard` Unix seconds.
    fn reading(heard: u64) -> Reading {
        Reading {
            left_battery: Some(80),
            right_battery: Some(75),
            case_battery: None,
            left_charging: false,
            right_charging: true,
            case_charging: false,
            identity: Identity {
                product_id: 0x2027,
                color: 0,
            },
            heard: UNIX_EPOCH + Duration::from_secs(heard),
        }
    }

    #[test]
    fn readings_round_trip_through_the_file() {
        let (store, dir) = store();
        let readings = HashMap::from([(MAC, reading(1_793_741_098)), (OTHER, reading(5))]);
        store.save(&readings).unwrap();

        assert_eq!(store.load().unwrap(), readings);
        let mode = std::fs::metadata(dir.path().join(READINGS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [READINGS_FILE], "no temporary file left behind");
    }

    /// Whole seconds are kept: a reading's age is shown in minutes at the
    /// finest.
    #[test]
    fn the_heard_time_is_kept_to_the_second() {
        let (store, _dir) = store();
        let mut heard = reading(1_793_741_098);
        heard.heard += Duration::from_millis(900);
        store.save(&HashMap::from([(MAC, heard)])).unwrap();
        assert_eq!(store.load().unwrap()[&MAC], reading(1_793_741_098));
    }

    #[test]
    fn the_file_holds_what_its_doc_says() {
        let (store, dir) = store();
        store
            .save(&HashMap::from([(MAC, reading(1_793_741_098))]))
            .unwrap();
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(READINGS_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            file,
            serde_json::json!({
                "version": 1,
                "readings": {
                    "00:00:5E:00:53:01": {
                        "left_battery": 80,
                        "right_battery": 75,
                        "case_battery": null,
                        "left_charging": false,
                        "right_charging": true,
                        "case_charging": false,
                        "product_id": 0x2027,
                        "color": 0,
                        "heard": 1_793_741_098u64,
                    }
                }
            })
        );
    }

    /// The file is only ours by its name: a level the parser would read as
    /// unavailable loads as unknown, never as a reading.
    #[test]
    fn a_level_above_100_loads_as_unknown() {
        let (store, _dir) = store();
        let mut stored = StoredReading::of(&reading(5));
        stored.left_battery = Some(101);
        stored.right_battery = Some(100);
        stored.case_battery = Some(0xFF);
        let mut file = serde_json::json!({"version": 1, "readings": {}});
        file["readings"][MAC.to_string()] = serde_json::to_value(stored).unwrap();
        std::fs::write(store.path(), file.to_string()).unwrap();

        let loaded = store.load().unwrap()[&MAC];
        assert_eq!(
            (
                loaded.left_battery,
                loaded.right_battery,
                loaded.case_battery
            ),
            (None, Some(100), None)
        );
    }

    #[test]
    fn no_file_is_no_readings() {
        let (store, _dir) = store();
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_and_moves_aside() {
        let (store, dir) = store();
        std::fs::write(
            store.path(),
            b"{\"version\": 1, \"readings\": {\"nope\": {}}}",
        )
        .unwrap();
        assert!(store.load().is_err());

        let aside = store.set_aside().unwrap();
        assert!(!store.path().exists());
        assert!(aside.starts_with(dir.path()));
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("readings.json.corrupt-")
        );
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn a_mac_that_does_not_parse_is_an_error() {
        let (store, _dir) = store();
        let mut file = serde_json::json!({"version": 1, "readings": {}});
        file["readings"]["not a MAC"] =
            serde_json::to_value(StoredReading::of(&reading(5))).unwrap();
        std::fs::write(store.path(), file.to_string()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn nothing_changed_is_no_save() {
        let saved = HashMap::from([(MAC, reading(1000))]);
        assert!(!due(&saved, &saved, HEARD_REFRESH));
        assert!(!due(&saved, &saved, Duration::ZERO));
        assert!(!due(&HashMap::new(), &HashMap::new(), HEARD_REFRESH));
    }

    #[test]
    fn a_pair_come_or_gone_is_a_save() {
        let one = HashMap::from([(MAC, reading(1000))]);
        let two = HashMap::from([(MAC, reading(1000)), (OTHER, reading(1000))]);
        assert!(due(&one, &two, HEARD_REFRESH));
        assert!(due(&two, &one, HEARD_REFRESH));
        let other = HashMap::from([(OTHER, reading(1000))]);
        assert!(due(&one, &other, HEARD_REFRESH), "one gone, another come");
    }

    #[test]
    fn a_changed_level_flag_or_model_is_a_save() {
        let saved = HashMap::from([(MAC, reading(1000))]);
        let changes: [fn(&mut Reading); 4] = [
            |r| r.left_battery = Some(79),
            |r| r.case_battery = Some(50),
            |r| r.right_charging = false,
            |r| r.identity.color = 1,
        ];
        for change in changes {
            let mut changed = reading(1001);
            change(&mut changed);
            assert!(due(&saved, &HashMap::from([(MAC, changed)]), HEARD_REFRESH));
        }
    }

    /// A heard time alone is due once it is `HEARD_REFRESH` past the one
    /// saved, at the quit whatever it is, and when it is behind the one saved -
    /// the clock set back.
    #[test]
    fn a_heard_time_alone_saves_once_it_has_moved_far_enough() {
        let saved = HashMap::from([(MAC, reading(1000))]);
        let refresh = HEARD_REFRESH.as_secs();
        let heard = |at: u64| HashMap::from([(MAC, reading(at))]);

        assert!(!due(&saved, &heard(1060), HEARD_REFRESH));
        assert!(!due(&saved, &heard(1000 + refresh - 1), HEARD_REFRESH));
        assert!(due(&saved, &heard(1000 + refresh), HEARD_REFRESH));
        assert!(due(&saved, &heard(1060), Duration::ZERO), "the quit's");
        assert!(
            due(&saved, &heard(999), HEARD_REFRESH),
            "the clock set back"
        );
    }
}
