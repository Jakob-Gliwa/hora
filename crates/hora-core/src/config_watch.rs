//! How a change to `config.toml` reaches the supervisor: SIGHUP, and inotify
//! (through `notify`) on the file's directory.
//!
//! Watching the directory catches every way of saving - writing in place,
//! writing a temporary file and renaming it over, swapping a symlink as a
//! Kubernetes `ConfigMap` does - so any event there (except a read) asks for a
//! reload, and the supervisor's content comparison drops the ones that changed
//! nothing.
//!
//! Where inotify cannot work, Hora says so instead of silently never reloading
//! (see [`live_reload_issues`]): a file bind-mounted on its own (a container
//! mapping `config.toml` instead of its directory) gets no directory events and
//! goes stale when the host replaces it, and a network filesystem does not see
//! changes made on other machines. A watcher that misses a change anyway is
//! caught by the self-check, which compares the file with what was applied
//! ([`ConfigWatch`]).

use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::warn;

use crate::config::ConfigIssue;

/// Filesystems on which inotify does not report changes made by other machines.
const NETWORK_FILESYSTEMS: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "smbfs", "9p", "fuse.sshfs"];

/// What the supervisor applied and how to ask it for a reload; shared with the
/// self-check, which uses it to notice a change no file event announced.
#[derive(Debug)]
pub struct ConfigWatch {
    path: PathBuf,
    applied: AtomicU64,
    reload: mpsc::Sender<()>,
}

impl ConfigWatch {
    /// The config file being watched.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The supervisor applied (or rejected) this text; it is current until the file differs.
    pub fn applied(&self, raw: &str) {
        self.applied.store(fingerprint(raw), Ordering::Release);
    }

    /// Whether `raw` is the text last applied.
    #[must_use]
    pub fn is_applied(&self, raw: &str) -> bool {
        self.applied.load(Ordering::Acquire) == fingerprint(raw)
    }

    /// Ask the supervisor to re-read the file.
    pub fn request_reload(&self) {
        let _ = self.reload.try_send(());
    }
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, reload: mpsc::Sender<()>) -> Self {
        Self {
            path,
            applied: AtomicU64::new(0),
            reload,
        }
    }
}

pub(crate) fn fingerprint(raw: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    hasher.finish()
}

/// Start listening for reasons to reload `config_path`. Returns the channel the
/// supervisor reads, the shared [`ConfigWatch`], and every reason a change could
/// go unnoticed (a failed watcher, a single-file mount, a network filesystem).
#[must_use]
pub fn start(
    config_path: &Path,
) -> (
    mpsc::Receiver<()>,
    std::sync::Arc<ConfigWatch>,
    Vec<ConfigIssue>,
) {
    let (tx, rx) = mpsc::channel(8);
    spawn_sighup(tx.clone());

    let mut issues = live_reload_issues(config_path);
    match file_watcher(config_path, tx.clone()) {
        Ok(watcher) => {
            // The watcher stops once dropped, so keep it alive for the process.
            tokio::spawn(async move {
                let _watcher = watcher;
                std::future::pending::<()>().await;
            });
        }
        Err(err) => issues.push(issue(format!(
            "cannot be watched ({err}); edits are applied only on SIGHUP or restart"
        ))),
    }
    let watch = std::sync::Arc::new(ConfigWatch {
        path: config_path.to_path_buf(),
        applied: AtomicU64::new(0),
        reload: tx,
    });
    (rx, watch, issues)
}

fn issue(reason: String) -> ConfigIssue {
    ConfigIssue {
        item: "live reload".to_owned(),
        reason,
    }
}

/// Why inotify cannot see every change to `config_path`, read from the mount table.
#[must_use]
pub fn live_reload_issues(config_path: &Path) -> Vec<ConfigIssue> {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new(); // Not Linux: nothing to judge.
    };
    let resolved = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    mount_issues(&mountinfo, &resolved)
}

/// [`live_reload_issues`] for a given mount table. Pure, for the tests.
fn mount_issues(mountinfo: &str, file: &Path) -> Vec<ConfigIssue> {
    let mut issues = Vec::new();
    let mounts: Vec<(PathBuf, String)> = mountinfo.lines().filter_map(parse_mount).collect();
    if mounts.iter().any(|(point, _)| point == file) {
        issues.push(issue(
            "the file is mounted on its own (a container mapping config.toml instead of its \
             directory): edits do not arrive, and a file replaced on the host can no longer be \
             read until restart. Mount the directory instead, e.g. …/hora/config:/etc/hora"
                .to_owned(),
        ));
    }
    // The filesystem the file lives on: the longest mount point containing it
    // (the last one wins when mounts are stacked on the same point).
    let filesystem = mounts
        .iter()
        .filter(|(point, _)| file.starts_with(point))
        .max_by_key(|(point, _)| point.components().count())
        .map(|(_, fstype)| fstype.as_str());
    if let Some(fstype) = filesystem.filter(|fstype| NETWORK_FILESYSTEMS.contains(fstype)) {
        issues.push(issue(format!(
            "the file is on a network filesystem ({fstype}): edits made on another machine do \
             not arrive; send SIGHUP after editing, or keep the file on a local disk"
        )));
    }
    issues
}

/// One `/proc/self/mountinfo` line: (mount point, filesystem type).
fn parse_mount(line: &str) -> Option<(PathBuf, String)> {
    let (before, after) = line.split_once(" - ")?;
    let point = before.split(' ').nth(4)?;
    let fstype = after.split(' ').next()?;
    Some((PathBuf::from(unescape(point)), fstype.to_owned()))
}

/// Mount points escape space, tab, newline and backslash as `\ooo`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|octal| u8::from_str_radix(octal, 8).ok())
        {
            out.push(byte);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
fn spawn_sighup(tx: mpsc::Sender<()>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(hangup) => hangup,
            Err(err) => {
                warn!("cannot listen for SIGHUP: {err}");
                return;
            }
        };
        while hangup.recv().await.is_some() {
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_sighup(_tx: mpsc::Sender<()>) {}

fn file_watcher(config_path: &Path, tx: mpsc::Sender<()>) -> notify::Result<RecommendedWatcher> {
    // Resolve symlinks so we watch the real file's directory: a symlinked config
    // would otherwise point the watcher at the link's directory and miss edits to
    // the target. (Reads still go through the original path.)
    let resolved = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let directory = resolved
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let link_directory = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != directory);

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        match event {
            // Reading the file - our own reload read included - must never
            // trigger a reload, or the watcher feeds itself forever.
            Ok(event) if matches!(event.kind, EventKind::Access(_)) => return,
            Ok(event) if event.need_rescan() => {
                warn!("config watcher lost events (queue overflow); re-reading the file");
            }
            Ok(_) => {}
            Err(err) => warn!("config watcher error ({err}); re-reading the file"),
        }
        // Any other event in the directory may concern the file (a rename over
        // it, a symlink swap whose name differs): re-read and let the content
        // comparison decide. Non-blocking: a dropped redundant event is harmless.
        let _ = tx.try_send(());
    })?;
    watcher.watch(&directory, RecursiveMode::NonRecursive)?;
    // A symlinked config: the link's own directory too, where the link is swapped.
    if let Some(link_directory) = link_directory {
        watcher.watch(link_directory, RecursiveMode::NonRecursive)?;
    }
    // The file itself as well: the only events that reach a file mounted on its own.
    if resolved
        .file_name()
        .is_some_and(|name| name != OsStr::new(""))
    {
        let _ = watcher.watch(&resolved, RecursiveMode::NonRecursive);
    }
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:44 / /mnt/nfs rw shared:20 - nfs4 server:/export rw
";

    #[test]
    fn a_directory_on_a_local_disk_is_fine() {
        assert_eq!(mount_issues(HOST, Path::new("/etc/hora/config.toml")), []);
    }

    #[test]
    fn a_file_mounted_on_its_own_is_reported() {
        let container = "\
500 400 0:60 / / rw - overlay overlay rw
510 500 0:61 /appdata/hora/config.toml /etc/hora/config.toml rw - fuse.shfs shfs rw
";
        let issues = mount_issues(container, Path::new("/etc/hora/config.toml"));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].reason.contains("mounted on its own"),
            "{}",
            issues[0]
        );
        // FUSE (Unraid's shfs) delivers events for writes through it: not flagged.
        assert!(!issues[0].reason.contains("network"));
        let directory = container.replace(
            "/appdata/hora/config.toml /etc/hora/config.toml",
            "/appdata/hora/config /etc/hora",
        );
        assert_eq!(
            mount_issues(&directory, Path::new("/etc/hora/config.toml")),
            []
        );
    }

    #[test]
    fn a_network_filesystem_is_reported() {
        let issues = mount_issues(HOST, Path::new("/mnt/nfs/hora/config.toml"));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].reason.contains("network filesystem (nfs4)"),
            "{}",
            issues[0]
        );
    }

    #[test]
    fn escaped_mount_points_are_decoded() {
        let line = "41 22 0:45 / /srv/my\\040conf rw - ext4 /dev/sdb1 rw";
        assert_eq!(
            parse_mount(line),
            Some((PathBuf::from("/srv/my conf"), "ext4".to_owned()))
        );
    }

    #[tokio::test]
    async fn every_way_of_saving_triggers_a_reload() {
        let dir = std::env::temp_dir().join(format!("hora-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "a").unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let watcher = file_watcher(&path, tx).unwrap();
        let wait = async |rx: &mut mpsc::Receiver<()>| {
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            while rx.try_recv().is_ok() {}
            got.is_ok()
        };

        std::fs::write(&path, "b").unwrap();
        assert!(wait(&mut rx).await, "in-place write");

        let tmp = dir.join(".config.toml.tmp");
        std::fs::write(&tmp, "c").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert!(wait(&mut rx).await, "atomic replace");

        // After the replace the directory watch still fires for the new file.
        std::fs::write(&path, "d").unwrap();
        assert!(wait(&mut rx).await, "write after replace");
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_applied_text_is_remembered() {
        let (tx, _rx) = mpsc::channel(1);
        let watch = ConfigWatch::for_test(PathBuf::from("/x"), tx);
        watch.applied("one");
        assert!(watch.is_applied("one"));
        assert!(!watch.is_applied("two"));
    }
}
