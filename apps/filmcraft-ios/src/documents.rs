//! Managed document identities and asynchronous UIKit document services.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DOCUMENT: AtomicU64 = AtomicU64::new(0);

/// Keep the original extension (it selects the encoder), but never accept directories.
fn file_name(suggested: &str) -> Result<String, String> {
    let name = Path::new(suggested).file_name().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).ok_or("The document needs a file name")?;
    if matches!(name, "." | "..") || name.contains('\0') {
        return Err("Invalid document name".into());
    }
    Ok(name.to_owned())
}

fn reserve_path(root: &Path, suggested: &str) -> Result<PathBuf, String> {
    let name = file_name(suggested)?;
    let documents = root.join("documents");
    std::fs::create_dir_all(&documents).map_err(|e| e.to_string())?;
    for _ in 0..128 {
        let sequence = NEXT_DOCUMENT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
        let dir = documents.join(format!("{nanos}-{sequence}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir.join(name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Could not reserve a document identity".into())
}

/// Each import batch owns its newly created directories until all files are ready.
/// Dropping an unsuccessful batch removes snapshots and bookmark metadata together.
#[derive(Default)]
struct ReservationBatch {
    directories: Vec<PathBuf>,
    committed: bool,
}
impl ReservationBatch {
    fn reserve(&mut self, root: &Path, suggested: &str) -> Result<PathBuf, String> {
        let path = reserve_path(root, suggested)?;
        // reserve_path always creates a fresh parent before returning the file identity.
        if let Some(parent) = path.parent() {
            self.directories.push(parent.to_owned());
        }
        Ok(path)
    }
    fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for ReservationBatch {
    fn drop(&mut self) {
        if !self.committed {
            for directory in &self.directories {
                // These exact fresh directories belong to this batch, never preexisting files.
                let _ = std::fs::remove_dir_all(directory);
            }
        }
    }
}

/// Copy a regular file (never a directory or a device) to `to`, streaming.
fn copy_regular_file(from: &Path, to: &Path) -> Result<(), String> {
    if !std::fs::metadata(from).map_err(|e| e.to_string())?.is_file() {
        return Err("Only regular files can be copied".into());
    }
    std::fs::copy(from, to).map(|_| ()).map_err(|e| e.to_string())
}

fn bounded_read_limit(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > limit {
        return Err("File exceeds the allowed size".into());
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("File exceeds the allowed size".into());
    }
    Ok(bytes)
}

fn validate_managed_path(root: &Path, path: &Path) -> Result<(), String> {
    let documents = std::fs::canonicalize(root.join("documents")).map_err(|e| e.to_string())?;
    let parent = path.parent().ok_or("Missing managed document directory")?;
    let parent = std::fs::canonicalize(parent).map_err(|e| e.to_string())?;
    if parent.parent() != Some(documents.as_path()) || path.file_name().is_none() || path.is_symlink() {
        return Err("Use Save As to select an iPadOS document destination".into());
    }
    Ok(())
}

/// Polls (every half second) a file must hold still before it is pushed: a video export grows for
/// minutes and may pause between writes, so a change counts only once it has been quiet for a while.
const STEADY_POLLS: u32 = 4;

/// What a managed file looked like when last seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::symlink_metadata(path).ok().filter(|m| m.is_file())?;
    Some(Stamp { len: meta.len(), modified: meta.modified().ok() })
}

#[derive(Debug, Default)]
struct Watched {
    /// What the last poll saw.
    seen: Option<Stamp>,
    /// How many polls in a row saw it unchanged.
    steady: u32,
    /// What is already where the user keeps it (an import's snapshot, or a finished push).
    synced: Option<Stamp>,
    /// A push is under way.
    pushing: bool,
}

/// Notices when the app has written a managed document, so the host can take it to where the user keeps
/// it. A change counts once it holds still for two polls (the app replaces the file in one rename, but
/// may write more than once in a row). Pure bookkeeping: the caller does the pushing.
#[derive(Debug, Default)]
pub struct Tracker {
    files: std::collections::HashMap<PathBuf, Watched>,
    /// Reservation folders whose files are watched.
    dirs: std::collections::HashSet<PathBuf>,
}

impl Tracker {
    /// Watch `path`. `imported`: it is a snapshot of the user's file, so what is there needs no push.
    pub fn watch(&mut self, path: PathBuf, imported: bool) {
        let now = stamp(&path);
        let entry = self.files.entry(path).or_default();
        if imported {
            entry.seen = now;
            entry.synced = now;
        } else {
            // Unseen until the first poll: it has to hold still for a poll after that.
            entry.seen = None;
        }
    }

    /// Look for managed documents not watched yet. One with a destination (an import, or a document
    /// the user saved before) is in sync; a new one is the app's own Save As, to be taken to where
    /// the user chooses once it is written. At `startup`, files left without a destination (the
    /// user declined the picker last run) count as in sync too, and empty reservations (a cancelled
    /// Save As) are removed.
    pub fn rescan(&mut self, root: &Path, startup: bool) {
        let Ok(dirs) = std::fs::read_dir(root.join("documents")) else { return };
        for dir in dirs.flatten().map(|d| d.path()) {
            if self.dirs.contains(&dir) || !dir.is_dir() {
                continue;
            }
            let files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|f| f.path())
                        .filter(|f| f.is_file() && !f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')))
                        .collect()
                })
                .unwrap_or_default();
            if files.is_empty() {
                if startup {
                    // Fails unless the folder is empty (a leftover `metadata` folder keeps it).
                    let _ = std::fs::remove_dir(&dir);
                }
                continue;
            }
            let has_destination = dir.join("metadata").join("destination.bookmark").is_file();
            for file in files {
                self.watch(file, has_destination || startup);
            }
            self.dirs.insert(dir);
        }
    }

    /// The files to push now: changed since they were last in sync and unchanged since the last poll.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut due = Vec::new();
        for (path, watched) in &mut self.files {
            let now = stamp(path);
            watched.steady = if now == watched.seen { watched.steady.saturating_add(1) } else { 0 };
            watched.seen = now;
            if watched.steady >= STEADY_POLLS && !watched.pushing && now.is_some() && now != watched.synced {
                watched.pushing = true;
                due.push(path.clone());
            }
        }
        due
    }

    /// A push of `path` ended. Done or declined, what was pushed counts as in sync (a declined export
    /// asks again at the next save, not every poll); a failure is tried again.
    pub fn finished(&mut self, path: &Path, handled: bool) {
        if let Some(watched) = self.files.get_mut(path) {
            watched.pushing = false;
            if handled {
                watched.synced = stamp(path);
            }
        }
    }
}

#[cfg(target_os = "ios")]
#[allow(unsafe_code)]
mod native;
#[cfg(target_os = "ios")]
pub use native::install;

/// Takes the managed documents the app saves to where the user keeps them ([`Tracker`], and a worker
/// that may wait for the export picker).
#[cfg(target_os = "ios")]
pub struct DocumentSync {
    root: PathBuf,
    tracker: Tracker,
    last_poll: std::time::Instant,
    done: std::sync::mpsc::Receiver<(PathBuf, Result<bool, String>)>,
    report: std::sync::mpsc::Sender<(PathBuf, Result<bool, String>)>,
}

#[cfg(target_os = "ios")]
impl DocumentSync {
    const EVERY: std::time::Duration = std::time::Duration::from_millis(500);

    pub fn new(root: PathBuf) -> Self {
        let mut tracker = Tracker::default();
        tracker.rescan(&root, true);
        let (report, done) = std::sync::mpsc::channel();
        Self { root, tracker, last_poll: std::time::Instant::now(), done, report }
    }

    /// Call every frame. `status` gets a message to show when a push fails.
    pub fn poll(&mut self, mut status: impl FnMut(String)) {
        while let Ok((path, result)) = self.done.try_recv() {
            match result {
                // Pushed or declined: what was there is handled (a decline asks again at the next save).
                Ok(_) => self.tracker.finished(&path, true),
                Err(error) => {
                    log::error!("Could not take {} to its destination: {error}", path.display());
                    status(format!("Could not save to your chosen location: {error}"));
                    self.tracker.finished(&path, false);
                }
            }
        }
        if self.last_poll.elapsed() < Self::EVERY {
            return;
        }
        self.last_poll = std::time::Instant::now();
        self.tracker.rescan(&self.root, false);
        for path in self.tracker.poll() {
            let (root, report) = (self.root.clone(), self.report.clone());
            let worker = std::thread::Builder::new().name("filmcraft-document-sync".into()).spawn({
                let path = path.clone();
                move || {
                    let _ = report.send((path.clone(), native::push(&root, &path)));
                }
            });
            if let Err(error) = worker {
                log::error!("Could not start the document sync: {error}");
                self.tracker.finished(&path, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("filmcraft-tracker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        root
    }

    /// Poll until a change would count as steady; every file due along the way.
    fn quiet(tracker: &mut Tracker) -> Vec<PathBuf> {
        (0..=STEADY_POLLS).flat_map(|_| tracker.poll()).collect()
    }

    #[test]
    fn a_new_document_is_pushed_once_it_holds_still() {
        let root = temp("new");
        let path = reserve_path(&root, "export.mp4").expect("reservation");
        let mut tracker = Tracker::default();
        tracker.watch(path.clone(), false);
        assert!(quiet(&mut tracker).is_empty(), "nothing written yet");
        std::fs::write(&path, b"first").expect("write");
        assert!(tracker.poll().is_empty(), "just changed: wait for it to hold still");
        assert_eq!(quiet(&mut tracker), vec![path.clone()]);
        assert!(quiet(&mut tracker).is_empty(), "a push is under way");
        tracker.finished(&path, true);
        assert!(quiet(&mut tracker).is_empty(), "in sync");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_file_still_growing_is_not_pushed() {
        let root = temp("growing");
        let path = reserve_path(&root, "export.mp4").expect("reservation");
        let mut tracker = Tracker::default();
        tracker.watch(path.clone(), false);
        for step in 0..(STEADY_POLLS * 3) {
            std::fs::write(&path, vec![1u8; 100 + step as usize]).expect("an export writes more");
            assert!(tracker.poll().is_empty(), "poll {step}: still being written");
        }
        assert_eq!(quiet(&mut tracker), vec![path]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn an_import_is_in_sync_until_the_app_saves_over_it() {
        let root = temp("import");
        let path = reserve_path(&root, "project.fcproj").expect("reservation");
        std::fs::write(&path, b"original").expect("write");
        let mut tracker = Tracker::default();
        tracker.watch(path.clone(), true);
        assert!(quiet(&mut tracker).is_empty());
        std::fs::write(&path, b"edited by the app").expect("save");
        assert_eq!(quiet(&mut tracker), vec![path.clone()]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_failed_push_is_tried_again_and_a_declined_one_waits_for_the_next_save() {
        let root = temp("retry");
        let path = reserve_path(&root, "a.fcproj").expect("reservation");
        std::fs::write(&path, b"v1").expect("write");
        let mut tracker = Tracker::default();
        tracker.watch(path.clone(), false);
        assert_eq!(quiet(&mut tracker), vec![path.clone()]);
        tracker.finished(&path, false);
        assert_eq!(quiet(&mut tracker), vec![path.clone()], "a failure is retried");
        tracker.finished(&path, true);
        assert!(quiet(&mut tracker).is_empty(), "a declined export is not asked again for the same bytes");
        std::fs::write(&path, b"version two").expect("save again");
        assert_eq!(quiet(&mut tracker), vec![path.clone()]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rescan_tells_imports_from_new_files_and_leftovers() {
        let root = temp("rescan");
        let kept = reserve_path(&root, "kept.fcproj").expect("reservation");
        std::fs::write(&kept, b"kept").expect("write");
        let metadata = kept.parent().expect("dir").join("metadata");
        std::fs::create_dir_all(&metadata).expect("metadata");
        std::fs::write(metadata.join("destination.bookmark"), b"bookmark").expect("bookmark");
        let declined = reserve_path(&root, "declined.mp4").expect("reservation");
        std::fs::write(&declined, b"never exported").expect("write");
        let cancelled = reserve_path(&root, "cancelled.fcproj").expect("reservation");
        let partial = reserve_path(&root, "clip.mov").expect("reservation");
        std::fs::write(partial.with_file_name(".clip.mov.part"), b"half a copy").expect("partial");
        let mut tracker = Tracker::default();
        tracker.rescan(&root, true);
        assert!(!cancelled.parent().expect("dir").exists(), "an empty reservation is removed at start");
        assert!(quiet(&mut tracker).is_empty(), "everything found at start is in sync");
        // During the run, a Save As reserves a folder and the app writes into it later.
        let fresh = reserve_path(&root, "fresh.fcproj").expect("reservation");
        tracker.rescan(&root, false);
        assert!(quiet(&mut tracker).is_empty());
        std::fs::write(&fresh, b"project").expect("save");
        tracker.rescan(&root, false);
        assert_eq!(quiet(&mut tracker), vec![fresh]);
        // The app saving over an opened project is pushed too; a partial copy never is.
        std::fs::write(&kept, b"edited").expect("edit");
        assert_eq!(quiet(&mut tracker), vec![kept]);
        assert!(!declined.as_os_str().is_empty());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn names_keep_encoder_extension_and_strip_directories() {
        assert_eq!(file_name("/external/保存.PSD").expect("name"), "保存.PSD");
        assert!(file_name("/").is_err());
        assert!(file_name("bad\0.svg").is_err());
    }
    #[test]
    fn reservations_never_create_empty_export_files() {
        let root = std::env::temp_dir().join(format!("filmcraft-documents-test-{}", std::process::id()));
        let first = reserve_path(&root, "picture.svg").expect("first reservation");
        let second = reserve_path(&root, "picture.svg").expect("second reservation");
        assert_ne!(first, second);
        assert!(!first.exists());
        std::fs::write(&first, b"real file bytes").expect("write fixture");
        assert_eq!(bounded_read_limit(&first, 1024).expect("bounded read"), b"real file bytes");
        assert!(bounded_read_limit(&first, 3).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn copies_stream_whole_files_and_refuse_directories() {
        let root = temp("copy");
        let from = reserve_path(&root, "clip.mp4").expect("reservation");
        let payload = vec![7u8; 3 * 1024 * 1024 + 5];
        std::fs::write(&from, &payload).expect("write");
        let to = from.with_file_name("copy.mp4");
        copy_regular_file(&from, &to).expect("copy");
        assert_eq!(std::fs::read(&to).expect("read"), payload);
        assert!(copy_regular_file(from.parent().expect("dir"), &root.join("x")).is_err());
        assert!(validate_managed_path(&root, &from).is_ok());
        assert!(validate_managed_path(&root, &root.join("documents/../prefs.json")).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn failed_second_import_rolls_back_completed_and_current_reservations() {
        let root = std::env::temp_dir().join(format!("filmcraft-import-rollback-test-{}", std::process::id()));
        let mut paths = Vec::new();
        let result = (|| -> Result<(), String> {
            let mut batch = ReservationBatch::default();
            let first = batch.reserve(&root, "first.svg")?;
            paths.push(first.clone());
            std::fs::write(&first, b"completed snapshot").map_err(|e| e.to_string())?;
            let second = batch.reserve(&root, "second.svg")?;
            paths.push(second.clone());
            std::fs::create_dir(second.parent().ok_or("parent")?.join("metadata")).map_err(|e| e.to_string())?;
            Err("synthetic bookmark or snapshot failure".into())
        })();
        assert!(result.is_err());
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|path| path.parent().is_some_and(|parent| !parent.exists())));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn successful_import_batch_preserves_every_reservation() {
        let root = std::env::temp_dir().join(format!("filmcraft-import-commit-test-{}", std::process::id()));
        let mut batch = ReservationBatch::default();
        let first = batch.reserve(&root, "first.svg").expect("first");
        let second = batch.reserve(&root, "second.svg").expect("second");
        std::fs::write(&first, b"first snapshot").expect("first snapshot");
        std::fs::write(&second, b"second snapshot").expect("second snapshot");
        batch.commit();
        assert!(first.is_file() && second.is_file());
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
