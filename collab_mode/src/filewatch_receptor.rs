// Receptor that watches the filesystem for changes to files from
// external processes and notify the main server.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{select, Receiver};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, Notify};
use tracing::trace;

/// Messages relate to file-watching.
#[derive(Debug)]
pub enum WatchFileMessage {
    /// Upon receiving this, receptor reconfigures the underlying
    /// notify watcher to watch exactly these files (actually, watch
    /// these files’ parent dirs to handle write-then-swap kind of
    /// behavior).
    WatchFiles { paths: Vec<PathBuf> },
    /// Receptor uses this to notify server that a file changed at
    /// what time.
    FileChanged { path: PathBuf, timestamp: Instant },
}

/// Capacity for the channel between the notify callback (sync thread
/// owned by notify) and our receptor loop.
const NOTIFY_CHAN_CAP: usize = 256;

/// Run the filewatch receptor. Returns when either channel closes.
/// Normally this should never return; notifies `shutdown` on exit
/// which the whole program.
pub fn run(
    msg_tx: mpsc::Sender<WatchFileMessage>,
    msg_rx: Receiver<WatchFileMessage>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    tracing::info!("Started filewatch receptor");

    let (notify_tx, notify_rx) =
        crossbeam_channel::bounded::<notify::Result<notify::Event>>(NOTIFY_CHAN_CAP);
    let mut watcher = notify::recommended_watcher(move |ev| {
        let _ = notify_tx.send(ev);
    })?;
    let mut watched_files: HashSet<PathBuf> = HashSet::new();
    let mut watched_dirs: HashSet<PathBuf> = HashSet::new();

    loop {
        select! {
            recv(notify_rx) -> ev => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(_) => {
                        tracing::warn!("notify channel closed; receptor exiting");
                        break;
                    }
                };
                if let Err(err) = handle_notify_event(ev, &watched_files, &msg_tx) {
                    tracing::warn!("filewatch receptor exiting: {}", err);
                    break;
                }
            }
            recv(msg_rx) -> msg => {
                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!("WatchFiles channel closed ({}); receptor exiting", err);
                        break;
                    }
                };
                match msg {
                    WatchFileMessage::WatchFiles { paths } => {
                        apply_watch_files(
                            paths,
                            &mut watcher,
                            &mut watched_files,
                            &mut watched_dirs,
                        );
                    }
                    WatchFileMessage::FileChanged { .. } => {}
                }
            }
        }
    }

    shutdown.notify_waiters();
    Ok(())
}

/// Filter, match against the watched set, and forward `FileChanged`
/// for each match. Returns `Err` only when the outbound channel to
/// the server has closed (caller should exit the loop). Non-fatal
/// conditions (a notify error, an unmatched path) are logged or
/// silently skipped and yield `Ok(())`.
fn handle_notify_event(
    event: notify::Result<notify::Event>,
    watched_files: &HashSet<PathBuf>,
    msg_tx: &mpsc::Sender<WatchFileMessage>,
) -> anyhow::Result<()> {
    let event = match event {
        Ok(ev) => ev,
        Err(err) => {
            tracing::warn!("notify reported error: {:?}", err);
            return Ok(());
        }
    };
    // All operations we care about involve at least one
    // Modify(Data(_)) event. See bin/test_filewatch.rs.
    if !matches!(event.kind, EventKind::Modify(ModifyKind::Data(_))) {
        return Ok(());
    }
    let timestamp = Instant::now();
    for path in event.paths {
        if !watched_files.contains(&path) {
            continue;
        }
        trace!("filewatch event: {:?} {:?}", event.kind, path);
        msg_tx
            .blocking_send(WatchFileMessage::FileChanged { path, timestamp })
            .map_err(|err| anyhow::anyhow!("outbound channel closed: {:?}", err))?;
    }
    Ok(())
}

/// Reconfigure the watcher to watch exactly the parent dirs of
/// `paths` (canonicalized). Updates the owned `watched_files` and
/// `watched_dirs` sets in place.
fn apply_watch_files(
    paths: Vec<PathBuf>,
    watcher: &mut RecommendedWatcher,
    watched_files: &mut HashSet<PathBuf>,
    watched_dirs: &mut HashSet<PathBuf>,
) {
    // Canonicalize each requested path so they match the form notify
    // reports events under (eg mac maps /tmp to /private/tmp). Drop
    // any path that can’t be canonicalized (should be very rare).
    let new_files: HashSet<PathBuf> = paths
        .into_iter()
        .filter_map(|p| match std::fs::canonicalize(&p) {
            Ok(c) => Some(c),
            Err(err) => {
                tracing::warn!("Failed to canonicalize watch path {:?}: {}", p, err);
                None
            }
        })
        .collect();
    let new_dirs: HashSet<PathBuf> = new_files
        .iter()
        .filter_map(|p| p.parent().map(PathBuf::from))
        .collect();

    // Configure the watched paths.
    let mut paths = watcher.paths_mut();
    for dir in watched_dirs.difference(&new_dirs) {
        if let Err(err) = paths.remove(dir) {
            tracing::warn!("Failed to unwatch {:?}: {}", dir, err);
        }
    }
    for dir in new_dirs.difference(watched_dirs) {
        if let Err(err) = paths.add(dir, RecursiveMode::NonRecursive) {
            tracing::warn!("Failed to watch {:?}: {}", dir, err);
        }
    }
    if let Err(err) = paths.commit() {
        tracing::warn!("Failed to commit watch changes: {}", err);
    }
    *watched_dirs = new_dirs;
    *watched_files = new_files;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// Spawn the receptor on a system thread and return the channel
    /// endpoints the test side keeps: a crossbeam `Sender` to feed
    /// WatchFiles, a tokio `Receiver` to consume FileChanged, and the
    /// shutdown notify.
    fn spawn_receptor() -> (
        crossbeam_channel::Sender<WatchFileMessage>,
        mpsc::Receiver<WatchFileMessage>,
        Arc<Notify>,
    ) {
        // server → receptor
        let (server_to_receptor_tx, server_to_receptor_rx) =
            crossbeam_channel::bounded::<WatchFileMessage>(16);
        // receptor → server
        let (receptor_to_server_tx, receptor_to_server_rx) = mpsc::channel(16);
        let shutdown = Arc::new(Notify::new());
        let shutdown_for_run = shutdown.clone();
        std::thread::spawn(move || {
            if let Err(err) = run(
                receptor_to_server_tx,
                server_to_receptor_rx,
                shutdown_for_run,
            ) {
                tracing::warn!("test receptor exited: {}", err);
            }
        });
        (server_to_receptor_tx, receptor_to_server_rx, shutdown)
    }

    /// notify’s OS layer is async wrt our writes; give it a moment to
    /// deliver each event before checking. The receptor’s
    /// `Instant::now()` stamp is captured at notify-deliver time, so
    /// short sleeps after a filesystem write are unavoidable here.
    const EVENT_WAIT: Duration = Duration::from_secs(5);
    const NEGATIVE_WAIT: Duration = Duration::from_millis(500);

    #[tokio::test]
    async fn watch_then_modify_emits_filechanged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "initial").unwrap();

        let (tx, mut rx, _shutdown) = spawn_receptor();
        tx.send(WatchFileMessage::WatchFiles {
            paths: vec![path.clone()],
        })
        .unwrap();
        // Give the watcher a beat to register. macOS FSEvents in
        // particular needs a noticeable lead time before its stream
        // starts delivering events to user-space.
        tokio::time::sleep(Duration::from_millis(500)).await;

        std::fs::write(&path, "second").unwrap();

        let event = timeout(EVENT_WAIT, rx.recv()).await.unwrap().unwrap();
        let expected = std::fs::canonicalize(&path).unwrap();
        match event {
            WatchFileMessage::FileChanged { path: got_path, .. } => {
                assert_eq!(got_path, expected)
            }
            WatchFileMessage::WatchFiles { .. } => panic!("expected FileChanged"),
        }
    }

    #[tokio::test]
    async fn modify_unwatched_sibling_no_event() {
        let dir = TempDir::new().unwrap();
        let watched = dir.path().join("a.txt");
        let sibling = dir.path().join("b.txt");
        std::fs::write(&watched, "a").unwrap();
        std::fs::write(&sibling, "b").unwrap();

        let (tx, mut rx, _shutdown) = spawn_receptor();
        tx.send(WatchFileMessage::WatchFiles {
            paths: vec![watched.clone()],
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        std::fs::write(&sibling, "b2").unwrap();

        // No event should arrive for the sibling.
        let got = timeout(NEGATIVE_WAIT, rx.recv()).await;
        assert!(got.is_err(), "unexpected event: {:?}", got);
    }

    #[tokio::test]
    async fn unwatch_via_empty_list() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").unwrap();

        let (tx, mut rx, _shutdown) = spawn_receptor();
        tx.send(WatchFileMessage::WatchFiles {
            paths: vec![path.clone()],
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Now unwatch.
        tx.send(WatchFileMessage::WatchFiles { paths: vec![] })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        std::fs::write(&path, "a2").unwrap();

        let got = timeout(NEGATIVE_WAIT, rx.recv()).await;
        assert!(got.is_err(), "unexpected event: {:?}", got);
    }

    // eqNeeds to run in single thread, hence the ignore.
    #[tokio::test]
    #[ignore]
    async fn atomic_rename_into_watched_name() {
        let dir = TempDir::new().unwrap();
        let watched = dir.path().join("a.txt");
        let tmp = dir.path().join("a.txt.tmp");
        std::fs::write(&watched, "original").unwrap();

        let (tx, mut rx, _shutdown) = spawn_receptor();
        tx.send(WatchFileMessage::WatchFiles {
            paths: vec![watched.clone()],
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Simulate atomic-rename: write to .tmp, then rename onto a.txt.
        std::fs::write(&tmp, "rewritten").unwrap();
        std::fs::rename(&tmp, &watched).unwrap();

        let event = timeout(EVENT_WAIT, rx.recv()).await.unwrap().unwrap();
        let expected = std::fs::canonicalize(&watched).unwrap();
        match event {
            WatchFileMessage::FileChanged { path: got_path, .. } => {
                assert_eq!(got_path, expected)
            }
            WatchFileMessage::WatchFiles { .. } => panic!("expected FileChanged"),
        }
    }
}
