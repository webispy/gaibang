//! State shared by every process on the machine, in one directory. Per pace key (a source, or a group of
//! sources behind one CDN): the next free request slot, a cool-down after a block, and counts of plain 403s and
//! of failures in a row; every change is made under the key's exclusive file lock. Per host: robots.txt cached
//! for ROBOTS_TTL (or that it could not be read, for ROBOTS_FAILURE_TTL). Per feed: validators for conditional
//! GET. Anything that cannot be read or written is an error, and the fetcher then sends nothing (fail closed).

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::Clock;

pub const ROBOTS_TTL_SECS: i64 = 24 * 3600;
pub const ROBOTS_FAILURE_TTL_SECS: i64 = 3600;
/// No wait is ever longer than this (a slot far in the future means a bad clock or a bad file).
pub const MAX_WAIT_SECS: u64 = 600;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cooldown {
    pub until: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Validators {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CachedRobots {
    /// The host's robots.txt.
    Text(String),
    /// No robots.txt (a 4xx other than 401/403/429): everything allowed.
    Missing,
    /// It could not be read (no answer, 5xx, a redirect off the host): the host is left alone for a while.
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RobotsEntry {
    fetched_at: i64,
    robots: CachedRobots,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KeyState {
    /// Milliseconds: the next request may start at this time.
    next_slot: Option<i64>,
    cooldown: Option<Cooldown>,
    strikes: u32,
    failures: u32,
}

pub struct State {
    dir: PathBuf,
}

fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// FNV-1a, for file names of long keys (feed URLs): stable across builds.
fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn io(path: &Path, error: std::io::Error) -> String {
    format!("{}: {error}", path.display())
}

impl State {
    /// `dir` must be absolute so that every caller, whatever its working directory, shares one state.
    pub fn open(dir: &Path) -> Result<State> {
        if !dir.is_absolute() {
            return Err(format!("{}: the state directory must be an absolute path", dir.display()));
        }
        for sub in ["keys", "robots", "feeds", "credentials"] {
            let path = dir.join(sub);
            fs::create_dir_all(&path).map_err(|e| io(&path, e))?;
        }
        let credentials = dir.join("credentials");
        fs::set_permissions(&credentials, fs::Permissions::from_mode(0o700)).map_err(|e| io(&credentials, e))?;
        Ok(State { dir: dir.to_path_buf() })
    }

    /// Run `f` on the key's state under its exclusive lock; the new state is written before the lock is released.
    /// The key's state; a missing file is a key never used. An empty or damaged file is an error: nothing is
    /// written in place (see `with_key`), so it cannot come from a crash and must not read as "no cool-down".
    fn read_key(&self, key: &str) -> Result<KeyState> {
        let path = self.dir.join("keys").join(format!("{}.json", safe(key)));
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: unreadable state: {e}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(KeyState::default()),
            Err(e) => Err(io(&path, e)),
        }
    }

    /// Change the key's state under its exclusive lock (a separate .lock file); the new state replaces the old
    /// by rename, so a process killed half-way leaves the old state, never an empty one.
    fn with_key<T>(&self, key: &str, f: impl FnOnce(&mut KeyState) -> T) -> Result<T> {
        let lock_path = self.dir.join("keys").join(format!("{}.lock", safe(key)));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| io(&lock_path, e))?;
        lock.lock().map_err(|e| io(&lock_path, e))?;
        let result = (|| {
            let mut state = self.read_key(key)?;
            let result = f(&mut state);
            let bytes = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
            write_atomic(&self.dir.join("keys").join(format!("{}.json", safe(key))), &bytes)?;
            Ok(result)
        })();
        lock.unlock().map_err(|e| io(&lock_path, e))?;
        result
    }

    /// Take the key's next slot, `interval` after the previous one taken by any process, and sleep until it.
    pub fn wait_turn(&self, key: &str, interval: Duration, clock: &dyn Clock) -> Result<()> {
        self.wait_turn_within(key, interval, None, clock).map(|_| ())
    }

    /// As `wait_turn`, but when the slot is more than `max_wait` away nothing is taken and Ok(false) returned
    /// (an interactive reader gives up instead of holding a place in the queue).
    pub fn wait_turn_within(
        &self,
        key: &str,
        interval: Duration,
        max_wait: Option<Duration>,
        clock: &dyn Clock,
    ) -> Result<bool> {
        let interval_ms = interval.min(Duration::from_secs(MAX_WAIT_SECS)).as_millis() as i64;
        let max_ahead = 6 * (MAX_WAIT_SECS as i64) * 1000; // beyond an hour of queued turns: a clock jump
        let limit = max_wait.map(|w| w.as_millis() as i64);
        let slot = self.with_key(key, |state| {
            let now = clock.now_millis(); // under the lock: time spent waiting for it is not a time already waited
            let slot = match state.next_slot {
                Some(next) if next > now && next - now <= max_ahead => next,
                _ => now, // free, or a slot beyond any real wait (clock jump, bad file): start now
            };
            if limit.is_some_and(|l| slot - now > l) {
                return None;
            }
            state.next_slot = Some(slot.saturating_add(interval_ms));
            Some(slot)
        })?;
        let Some(slot) = slot else { return Ok(false) };
        let now = clock.now_millis();
        if slot > now {
            clock.sleep(Duration::from_millis((slot - now) as u64));
        }
        Ok(true)
    }

    /// Read-only: the cool-down in force, if any.
    pub fn cooldown(&self, key: &str, clock: &dyn Clock) -> Result<Option<Cooldown>> {
        let now = clock.now_millis() / 1000;
        Ok(self.read_key(key)?.cooldown.filter(|c| c.until > now))
    }

    /// Start (or extend, never shorten) a cool-down.
    pub fn set_cooldown(&self, key: &str, secs: i64, reason: &str, clock: &dyn Clock) -> Result<()> {
        let until = clock.now_millis() / 1000 + secs;
        self.with_key(key, |state| {
            if state.cooldown.as_ref().is_none_or(|c| c.until < until) {
                state.cooldown = Some(Cooldown { until, reason: reason.into() });
            }
        })
    }

    /// Count a plain 403; returns the count in a row.
    pub fn strike(&self, key: &str) -> Result<u32> {
        self.with_key(key, |state| {
            state.strikes += 1;
            state.strikes
        })
    }

    /// Count a failure without an answer or with a 5xx; returns the count in a row.
    pub fn failure(&self, key: &str) -> Result<u32> {
        self.with_key(key, |state| {
            state.failures += 1;
            state.failures
        })
    }

    /// An answer that was not a refusal or a failure: the counts in a row start again.
    pub fn answered(&self, key: &str) -> Result<()> {
        self.with_key(key, |state| (state.strikes, state.failures) = (0, 0))
    }

    pub fn robots(&self, host: &str, clock: &dyn Clock) -> Result<Option<CachedRobots>> {
        let path = self.dir.join("robots").join(format!("{}.json", safe(host)));
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&path, e)),
        };
        let Ok(entry) = serde_json::from_str::<RobotsEntry>(&text) else { return Ok(None) }; // refetched
        let ttl = if entry.robots == CachedRobots::Unavailable { ROBOTS_FAILURE_TTL_SECS } else { ROBOTS_TTL_SECS };
        Ok((clock.now_millis() / 1000 - entry.fetched_at < ttl).then_some(entry.robots))
    }

    pub fn save_robots(&self, host: &str, robots: CachedRobots, clock: &dyn Clock) -> Result<()> {
        let entry = RobotsEntry { fetched_at: clock.now_millis() / 1000, robots };
        let bytes = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
        write_atomic(&self.dir.join("robots").join(format!("{}.json", safe(host))), &bytes)
    }

    /// A credential an API issued (an access token), shared by every process: `f` gets the stored text and
    /// returns the text to store instead (None: keep it) and its result, all under one exclusive lock, so that
    /// one process issues while the others wait and then reuse it. Stored 0600 in a 0700 directory.
    pub fn with_credential<T>(&self, name: &str, f: impl FnOnce(Option<&str>) -> (Option<String>, T)) -> Result<T> {
        let dir = self.dir.join("credentials");
        let lock_path = dir.join(format!("{}.lock", safe(name)));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| io(&lock_path, e))?;
        lock.lock().map_err(|e| io(&lock_path, e))?;
        let path = dir.join(format!("{}.json", safe(name)));
        let result = (|| {
            let stored = match fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => return Err(io(&path, e)),
            };
            let (replace, result) = f(stored.as_deref());
            if let Some(text) = replace {
                write_private(&path, text.as_bytes())?;
            }
            Ok(result)
        })();
        lock.unlock().map_err(|e| io(&lock_path, e))?;
        result
    }

    fn feed_file(&self, url: &str) -> PathBuf {
        self.dir.join("feeds").join(format!("{}.json", hash(url)))
    }

    pub fn validators(&self, url: &str) -> Validators {
        fs::read_to_string(self.feed_file(url))
            .ok()
            .and_then(|t| serde_json::from_str::<Validators>(&t).ok())
            .filter(|v| v.url == url) // a hash collision is an unconditional GET, never a wrong 304
            .unwrap_or_default()
    }

    pub fn save_validators(&self, validators: &Validators) -> Result<()> {
        let bytes = serde_json::to_vec(validators).map_err(|e| e.to_string())?;
        write_atomic(&self.feed_file(&validators.url), &bytes)
    }
}

/// As write_atomic, readable by the owner only from the first byte.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| io(path, e))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| io(path, e))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::clock::FakeClock;

    /// A fresh directory per call: parallel tests sharing one would reset each other's counts.
    pub fn temp() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("gaibang-core-state-{}-{n}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_credential_is_shared_private_and_replaced_only_when_asked() {
        let dir = temp();
        let (a, b) = (State::open(&dir).unwrap(), State::open(&dir).unwrap());
        assert!(a.with_credential("kis", |old| (Some("t1".into()), old.is_none())).unwrap());
        assert_eq!(b.with_credential("kis", |old| (None, old.map(String::from))).unwrap().as_deref(), Some("t1"));
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(dir.join("credentials/kis.json")), mode(dir.join("credentials"))), (0o600, 0o700));
    }

    #[test]
    fn turns_are_spaced_by_the_interval_across_state_handles() {
        let dir = temp();
        let clock = FakeClock::new(1_000_000);
        let (a, b) = (State::open(&dir).unwrap(), State::open(&dir).unwrap()); // two processes
        a.wait_turn("k", Duration::from_secs(3), &clock).unwrap();
        b.wait_turn("k", Duration::from_secs(3), &clock).unwrap();
        a.wait_turn("other", Duration::from_secs(3), &clock).unwrap();
        assert_eq!(clock.slept(), vec![Duration::from_secs(3)]);
    }

    #[test]
    fn a_reader_with_a_wait_limit_gives_up_without_taking_a_slot() {
        let clock = FakeClock::new(1_000_000);
        let state = State::open(&temp()).unwrap();
        state.wait_turn("k", Duration::from_secs(3), &clock).unwrap();
        state.wait_turn("k", Duration::from_secs(3), &clock).unwrap(); // the next slot is now 3 s away
        let limit = Some(Duration::from_secs(2));
        assert!(!state.wait_turn_within("k", Duration::from_secs(3), limit, &clock).unwrap());
        assert_eq!(clock.slept(), vec![Duration::from_secs(3)]); // gave up without sleeping
        clock.advance(Duration::from_secs(2));
        assert!(state.wait_turn_within("k", Duration::from_secs(3), limit, &clock).unwrap()); // its place was not taken
    }

    #[test]
    fn a_huge_interval_still_waits_and_a_far_slot_is_not_trusted() {
        let clock = FakeClock::new(1_000_000);
        let state = State::open(&temp()).unwrap();
        state.wait_turn("k", Duration::from_secs(u64::MAX / 2), &clock).unwrap();
        state.wait_turn("k", Duration::from_secs(3), &clock).unwrap();
        assert_eq!(clock.slept(), vec![Duration::from_secs(MAX_WAIT_SECS)]);
    }

    #[test]
    fn cooldown_expires_and_is_never_shortened() {
        let clock = FakeClock::new(1_000_000);
        let state = State::open(&temp()).unwrap();
        state.set_cooldown("k", 100, "challenge", &clock).unwrap();
        state.set_cooldown("k", 10, "429", &clock).unwrap();
        assert_eq!(state.cooldown("k", &clock).unwrap().unwrap().reason, "challenge");
        clock.advance(Duration::from_secs(101));
        assert!(state.cooldown("k", &clock).unwrap().is_none());
    }

    #[test]
    fn an_unreadable_key_file_is_an_error_not_a_free_pass() {
        let dir = temp();
        let clock = FakeClock::new(1_000_000);
        let state = State::open(&dir).unwrap();
        fs::write(dir.join("keys").join("k.json"), "{not json").unwrap();
        assert!(state.cooldown("k", &clock).is_err());
        assert!(state.wait_turn("k", Duration::from_secs(3), &clock).is_err());
    }

    #[test]
    fn an_emptied_or_damaged_key_file_does_not_forget_a_cooldown() {
        let dir = temp();
        let clock = FakeClock::new(1_000_000);
        let state = State::open(&dir).unwrap();
        state.set_cooldown("k", 1000, "challenge", &clock).unwrap();
        let file = dir.join("keys").join("k.json");
        for damaged in ["", "{\"cooldown\":{\"until\":"] {
            fs::write(&file, damaged).unwrap();
            assert!(state.cooldown("k", &clock).is_err());
            assert!(state.wait_turn("k", Duration::from_secs(3), &clock).is_err());
        }
        // reading never rewrites the file
        fs::remove_file(&file).unwrap();
        state.cooldown("k", &clock).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn a_relative_state_directory_is_refused() {
        assert!(State::open(Path::new("fetch-state")).is_err());
    }

    #[test]
    fn counts_in_a_row_start_again_after_an_answer() {
        let state = State::open(&temp()).unwrap();
        assert_eq!(state.strike("k").unwrap(), 1);
        assert_eq!(state.failure("k").unwrap(), 1);
        assert_eq!(state.strike("k").unwrap(), 2);
        state.answered("k").unwrap();
        assert_eq!((state.strike("k").unwrap(), state.failure("k").unwrap()), (1, 1));
    }

    #[test]
    fn concurrent_strikes_and_cooldowns_lose_nothing() {
        let dir = temp();
        let threads: Vec<_> = (0..4)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let state = State::open(&dir).unwrap();
                    let clock = FakeClock::new(1_000_000);
                    for _ in 0..25 {
                        if i == 0 {
                            state.set_cooldown("k", 1000, "challenge", &clock).unwrap();
                        } else {
                            state.strike("k").unwrap();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let state = State::open(&dir).unwrap();
        assert!(state.cooldown("k", &FakeClock::new(1_000_000)).unwrap().is_some());
        assert_eq!(state.strike("k").unwrap(), 76);
    }

    #[test]
    fn robots_cache_lasts_a_day_and_a_failure_an_hour() {
        let clock = FakeClock::new(1_000_000);
        let state = State::open(&temp()).unwrap();
        state.save_robots("h", CachedRobots::Text("User-agent: *\n".into()), &clock).unwrap();
        state.save_robots("u", CachedRobots::Unavailable, &clock).unwrap();
        clock.advance(Duration::from_secs(ROBOTS_FAILURE_TTL_SECS as u64));
        assert!(state.robots("h", &clock).unwrap().is_some());
        assert!(state.robots("u", &clock).unwrap().is_none());
        clock.advance(Duration::from_secs(ROBOTS_TTL_SECS as u64));
        assert!(state.robots("h", &clock).unwrap().is_none());
    }

    #[test]
    fn feed_validators_are_keyed_by_the_whole_url() {
        let state = State::open(&temp()).unwrap();
        let v = Validators { url: "https://h/feed?a=1".into(), etag: Some("x".into()), last_modified: None };
        state.save_validators(&v).unwrap();
        assert_eq!(state.validators("https://h/feed?a=1"), v);
        assert_eq!(state.validators("https://h/feed?a=2"), Validators::default());
    }
}
