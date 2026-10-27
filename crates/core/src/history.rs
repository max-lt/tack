//! On-disk storage for conversations, so they can be resumed later.
//!
//! One JSON file per session under `<config dir>/sessions/<id>.json`. The
//! bootstrap messages (tool instructions) are not stored: they are rebuilt for
//! the current working directory when a session is restored.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::Message;
use crate::config::{self, make_private};
use crate::store::Store;

const PREVIEW_CHARS: usize = 80;

/// Format of the session files this ccrs writes. A file without a version
/// is format 1.
const FORMAT_VERSION: u64 = 1;

/// Largest session file that is read. Real sessions are well under 1 MiB.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    pub cwd: PathBuf,
    pub model: String,
    /// Seconds since the Unix epoch.
    pub updated_at: u64,
    pub messages: Vec<Message>,
    /// Conversation size after the last turn, in tokens. Absent in sessions
    /// saved before it was recorded; the session estimates it on restore.
    #[serde(default)]
    pub context_used: Option<u64>,
    /// What the session had set up: terminals, later monitors and schedules.
    #[serde(default, skip_serializing_if = "Store::is_empty")]
    pub store: Store,
}

/// What the resume picker shows for one saved session.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub updated_at: u64,
    pub message_count: usize,
    /// Start of the first user message.
    pub preview: String,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A fresh, sortable session id (milliseconds since the Unix epoch).
pub fn new_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        .to_string()
}

pub fn sessions_dir() -> Result<PathBuf> {
    let dir = config::config_dir()?.join("sessions");
    make_private(&dir).context("Failed to create sessions directory")?;
    Ok(dir)
}

pub fn save(session: &StoredSession) -> Result<()> {
    save_in(&sessions_dir()?, session)
}

pub fn load(id: &str) -> Result<StoredSession> {
    load_from(&sessions_dir()?, id)
}

/// The saved session `id`, or `None` when there is none yet. Unlike
/// [`load`], a missing file is not an error; a bad id still is.
pub fn find(id: &str) -> Result<Option<StoredSession>> {
    let path = session_path(&sessions_dir()?, id)?;

    if !path.exists() {
        return Ok(None);
    }

    load(id).map(Some)
}

/// Held while a process has the session open. Another process cannot
/// open it, or else the last one to save would erase the turns of the
/// other.
pub struct SessionLock {
    #[cfg(unix)]
    _file: fs::File,
}

pub fn lock(id: &str) -> Result<SessionLock> {
    lock_in(&sessions_dir()?, id)
}

fn lock_in(dir: &Path, id: &str) -> Result<SessionLock> {
    let path = session_path(dir, id)?.with_extension("lock");

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .context("Failed to open the session lock")?;

        // SAFETY: the descriptor is open for as long as `file` lives.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();

            if error.kind() == std::io::ErrorKind::WouldBlock {
                bail!("Session {id} is open in another ccrs process");
            }
            return Err(error).context("Failed to lock the session");
        }

        Ok(SessionLock { _file: file })
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(SessionLock {})
    }
}

/// Saved sessions started from `cwd`, most recent first.
pub fn list(cwd: &Path) -> Result<Vec<SessionSummary>> {
    list_in(&sessions_dir()?, cwd)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn session_path(dir: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("Invalid session id: {id:?}");
    }

    Ok(dir.join(format!("{id}.json")))
}

fn save_in(dir: &Path, session: &StoredSession) -> Result<()> {
    let path = session_path(dir, &session.id)?;
    // One name per write: two writers of one id must not share a file.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{}.{}.{n}.tmp", session.id, std::process::id()));
    let mut json = serde_json::to_value(session)?;
    json["version"] = FORMAT_VERSION.into();
    let contents = json.to_string();

    // Write then rename, so a crash mid-write never corrupts a saved session.
    let written = write_private(&tmp, contents.as_bytes())
        .context("Failed to write session file")
        .and_then(|()| fs::rename(&tmp, &path).context("Failed to replace session file"));

    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written?;

    Ok(())
}

/// Write a new file that only the owner can read.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    file.write_all(contents)?;
    // On disk before the rename, or else a crash can leave an empty file
    // under the final name.
    file.sync_all()
}

/// Read a session file, or fail when it is over `MAX_FILE_BYTES`. The size
/// is checked before the read, and the read stops at the limit too.
fn read_capped(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let too_large = || {
        std::io::Error::other(format!("the file is too large (over {MAX_FILE_BYTES} bytes)"))
    };

    let file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_FILE_BYTES {
        return Err(too_large());
    }

    let mut contents = String::new();
    file.take(MAX_FILE_BYTES + 1).read_to_string(&mut contents)?;

    if contents.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large());
    }

    Ok(contents)
}

fn load_from(dir: &Path, id: &str) -> Result<StoredSession> {
    let path = session_path(dir, id)?;

    if !path.exists() {
        bail!("No saved session with id {id}");
    }

    let contents = read_capped(&path).with_context(|| format!("Failed to read session {id}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&contents).with_context(|| format!("Session {id} is corrupted"))?;

    // A newer format can hold fields this ccrs does not know. A save would
    // erase them, so the file is not loaded.
    let version = json.get("version").map_or(Some(1), serde_json::Value::as_u64);
    match version {
        Some(v) if v <= FORMAT_VERSION => {}
        _ => bail!(
            "Session {id} was saved by a newer ccrs (format {}); this ccrs reads format {FORMAT_VERSION}",
            json["version"]
        ),
    }

    serde_json::from_value(json).with_context(|| format!("Session {id} is corrupted"))
}

fn list_in(dir: &Path, cwd: &Path) -> Result<Vec<SessionSummary>> {
    let mut sessions = Vec::new();

    for entry in fs::read_dir(dir).context("Failed to read sessions directory")? {
        let path = entry?.path();

        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }

        // Unreadable or foreign files are skipped, not fatal.
        let Ok(contents) = read_capped(&path) else {
            continue;
        };
        let Ok(stored) = serde_json::from_str::<StoredSession>(&contents) else {
            continue;
        };

        if stored.cwd != cwd {
            continue;
        }

        sessions.push(summarize(&stored));
    }

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));

    Ok(sessions)
}

fn summarize(stored: &StoredSession) -> SessionSummary {
    let first_user = stored
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.to_text())
        .find(|t| !t.trim().is_empty())
        .unwrap_or_default();

    let flat = first_user.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = if flat.chars().count() > PREVIEW_CHARS {
        let cut: String = flat.chars().take(PREVIEW_CHARS).collect();
        format!("{cut}…")
    } else {
        flat
    };

    SessionSummary {
        id: stored.id.clone(),
        updated_at: stored.updated_at,
        message_count: stored.messages.len(),
        preview,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Content;

    fn stored(id: &str, cwd: &str, updated_at: u64, first: &str) -> StoredSession {
        StoredSession {
            id: id.to_string(),
            cwd: PathBuf::from(cwd),
            model: "claude-sonnet-5-5".to_string(),
            updated_at,
            context_used: None,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: Content::text(first),
                },
                Message {
                    role: "assistant".to_string(),
                    content: Content::text("ok"),
                },
            ],
            store: Store::default(),
        }
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        save_in(tmp.path(), &stored("1", "/p", 10, "hello")).unwrap();

        let loaded = load_from(tmp.path(), "1").unwrap();

        assert_eq!(loaded.id, "1");
        assert_eq!(loaded.cwd, PathBuf::from("/p"));
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content.to_text(), "hello");
    }

    #[test]
    fn context_used_roundtrips_and_old_files_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = stored("1", "/p", 10, "hello");
        s.context_used = Some(1234);
        save_in(tmp.path(), &s).unwrap();
        assert_eq!(load_from(tmp.path(), "1").unwrap().context_used, Some(1234));

        // A file written before the field existed.
        let mut json: serde_json::Value = serde_json::to_value(stored("2", "/p", 10, "hi")).unwrap();
        json.as_object_mut().unwrap().remove("context_used");
        fs::write(tmp.path().join("2.json"), json.to_string()).unwrap();
        assert_eq!(load_from(tmp.path(), "2").unwrap().context_used, None);
    }

    #[test]
    fn load_missing_session_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = load_from(tmp.path(), "nope").unwrap_err();

        assert!(err.to_string().contains("No saved session"));
    }

    #[test]
    fn ids_cannot_escape_the_directory() {
        let tmp = tempfile::tempdir().unwrap();

        assert!(load_from(tmp.path(), "../credentials").is_err());
        assert!(load_from(tmp.path(), "").is_err());
    }

    fn write_raw(dir: &Path, id: &str, contents: &str) {
        fs::write(dir.join(format!("{id}.json")), contents).unwrap();
    }

    fn json_of(s: &StoredSession) -> serde_json::Value {
        serde_json::to_value(s).unwrap()
    }

    #[test]
    fn bad_json_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_raw(tmp.path(), "1", "{\"id\": \"1\", \"messages\": [");

        let err = load_from(tmp.path(), "1").unwrap_err();
        assert!(err.to_string().contains("Session 1 is corrupted"), "{err}");
    }

    #[test]
    fn an_empty_file_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_raw(tmp.path(), "1", "");

        let err = load_from(tmp.path(), "1").unwrap_err();
        assert!(err.to_string().contains("corrupted"), "{err}");
    }

    #[test]
    fn a_missing_required_field_is_an_error_and_an_optional_one_a_default() {
        let tmp = tempfile::tempdir().unwrap();

        let mut json = json_of(&stored("1", "/p", 10, "hi"));
        json.as_object_mut().unwrap().remove("messages");
        write_raw(tmp.path(), "1", &json.to_string());
        assert!(load_from(tmp.path(), "1").is_err());

        let mut json = json_of(&stored("2", "/p", 10, "hi"));
        json.as_object_mut().unwrap().remove("store");
        write_raw(tmp.path(), "2", &json.to_string());
        assert!(load_from(tmp.path(), "2").unwrap().store.is_empty());
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut json = json_of(&stored("1", "/p", 10, "hi"));
        json["from_the_future"] = serde_json::json!({ "x": 1 });
        write_raw(tmp.path(), "1", &json.to_string());

        assert_eq!(load_from(tmp.path(), "1").unwrap().messages.len(), 2);
    }

    #[test]
    fn a_cut_write_leaves_the_saved_session_intact() {
        let tmp = tempfile::tempdir().unwrap();
        save_in(tmp.path(), &stored("1", "/p", 10, "kept")).unwrap();
        // What a crash during the write of the temporary file leaves.
        fs::write(tmp.path().join("1.json.tmp"), "{\"id\": \"1\", \"mess").unwrap();

        let loaded = load_from(tmp.path(), "1").unwrap();
        assert_eq!(loaded.messages[0].content.to_text(), "kept");
        assert_eq!(list_in(tmp.path(), Path::new("/p")).unwrap().len(), 1);
    }

    #[test]
    fn ids_with_path_characters_are_refused_for_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();

        for id in ["../x", "a/b", "/abs", "a\\b", "..", "a.b", "a b"] {
            assert!(load_from(tmp.path(), id).is_err(), "load {id:?}");
            assert!(save_in(tmp.path(), &stored(id, "/p", 1, "x")).is_err(), "save {id:?}");
        }

        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn session_files_and_their_directory_are_for_the_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        fs::create_dir(&dir).unwrap();
        // A directory made before this rule.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        make_private(&dir).unwrap();
        save_in(&dir, &stored("1", "/p", 1, "secret")).unwrap();

        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("1.json")), 0o600);
    }

    #[test]
    fn two_writers_on_one_id_never_leave_a_broken_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        save_in(&dir, &stored("1", "/p", 1, "start")).unwrap();

        let writers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|who| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    // Large enough that one write takes several syscalls.
                    let body = who.repeat(200_000);
                    for _ in 0..50 {
                        save_in(&dir, &stored("1", "/p", 1, &body)).unwrap();
                    }
                })
            })
            .collect();

        let mut reads = 0;
        while writers.iter().any(|w| !w.is_finished()) {
            load_from(&dir, "1").expect("a reader saw a broken session file");
            reads += 1;
        }
        for w in writers {
            w.join().unwrap();
        }

        assert!(reads > 0);
        load_from(&dir, "1").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_session_is_open_in_one_process_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();

        let first = lock_in(tmp.path(), "1").unwrap();
        let err = lock_in(tmp.path(), "1").err().expect("second lock taken");
        assert!(err.to_string().contains("open in another ccrs process"), "{err}");

        // Another id is free, and the id is free again once released.
        lock_in(tmp.path(), "2").unwrap();
        drop(first);
        lock_in(tmp.path(), "1").unwrap();

        assert!(lock_in(tmp.path(), "../x").is_err());
        assert!(list_in(tmp.path(), Path::new("/p")).unwrap().is_empty());
    }

    #[test]
    fn a_file_from_a_newer_ccrs_is_refused_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let mut json = json_of(&stored("1", "/p", 10, "hi"));
        json["version"] = serde_json::json!(FORMAT_VERSION + 1);
        json["only_newer_ccrs_knows"] = serde_json::json!(true);
        write_raw(tmp.path(), "1", &json.to_string());

        let err = load_from(tmp.path(), "1").unwrap_err();
        assert!(err.to_string().contains("newer ccrs"), "{err}");
    }

    #[test]
    fn files_say_their_version_and_old_files_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        save_in(tmp.path(), &stored("1", "/p", 10, "hi")).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("1.json")).unwrap()).unwrap();
        assert_eq!(raw["version"], FORMAT_VERSION);

        let mut json = json_of(&stored("2", "/p", 10, "hi"));
        json.as_object_mut().unwrap().remove("version");
        write_raw(tmp.path(), "2", &json.to_string());
        load_from(tmp.path(), "2").unwrap();
    }

    #[test]
    fn a_file_over_the_size_limit_is_refused_without_a_read() {
        let tmp = tempfile::tempdir().unwrap();
        save_in(tmp.path(), &stored("ok", "/p", 1, "hi")).unwrap();
        // Sparse: no disk space, but the size says 1 byte over the limit.
        let big = fs::File::create(tmp.path().join("big.json")).unwrap();
        big.set_len(MAX_FILE_BYTES + 1).unwrap();

        let err = format!("{:#}", load_from(tmp.path(), "big").unwrap_err());
        assert!(err.contains("Failed to read session big: the file is too large"), "{err}");
        assert_eq!(list_in(tmp.path(), Path::new("/p")).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_for_the_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let _lock = lock_in(tmp.path(), "1").unwrap();

        assert_eq!(mode(&tmp.path().join("1.lock")), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_that_cannot_be_created_is_an_error_not_a_panic() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ro");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();

        let err = lock_in(&dir, "1").err().expect("lock in a read-only directory");
        assert!(format!("{err:#}").contains("Failed to open the session lock"), "{err:#}");

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn list_filters_by_cwd_and_sorts_recent_first() {
        let tmp = tempfile::tempdir().unwrap();
        save_in(tmp.path(), &stored("a", "/p", 10, "old")).unwrap();
        save_in(tmp.path(), &stored("b", "/p", 30, "new")).unwrap();
        save_in(tmp.path(), &stored("c", "/other", 20, "elsewhere")).unwrap();

        let found = list_in(tmp.path(), Path::new("/p")).unwrap();
        let ids: Vec<_> = found.iter().map(|s| s.id.as_str()).collect();

        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn list_skips_garbage_files() {
        let tmp = tempfile::tempdir().unwrap();
        save_in(tmp.path(), &stored("a", "/p", 10, "hi")).unwrap();
        fs::write(tmp.path().join("broken.json"), "not json").unwrap();
        fs::write(tmp.path().join("notes.txt"), "hello").unwrap();

        let found = list_in(tmp.path(), Path::new("/p")).unwrap();

        assert_eq!(found.len(), 1);
    }

    #[test]
    fn preview_is_flattened_and_truncated() {
        let long = format!("line one\n{}", "x".repeat(200));
        let summary = summarize(&stored("a", "/p", 1, &long));

        assert!(summary.preview.starts_with("line one xxx"));
        assert!(summary.preview.ends_with('…'));
        assert_eq!(summary.preview.chars().count(), PREVIEW_CHARS + 1);
        assert_eq!(summary.message_count, 2);
    }
}
