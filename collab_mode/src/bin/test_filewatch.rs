//! Probe the OS file-watch event stream. Runs a fixed set of
//! filesystem operations against a `notify::recommended_watcher`
//! pointed at a temp directory, and prints every event that comes
//! back with its latency relative to the op start. Use this whenever
//! `collab_mode`'s filewatch behavior looks off on a platform, or
//! when validating behavior on a new one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

struct Probe {
    dir: PathBuf,
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    _watcher: RecommendedWatcher,
}

impl Probe {
    /// Run a single scenario: drain stale events, print a header,
    /// time the op, then drain events for a 1s window.
    fn scenario<F>(&self, name: &str, op: F) -> anyhow::Result<()>
    where
        F: FnOnce(&Path) -> anyhow::Result<()>,
    {
        while self.rx.try_recv().is_ok() {}

        println!("\n========================================");
        println!("Scenario: {}", name);
        println!("========================================");

        let start = Instant::now();
        op(&self.dir)?;

        let window = Duration::from_secs(2);
        let deadline = start + window;
        let mut count = 0;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(Ok(ev)) => {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    let paths: Vec<String> = ev
                        .paths
                        .iter()
                        .map(|p| {
                            p.strip_prefix(&self.dir)
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|_| p.display().to_string())
                        })
                        .collect();
                    println!("  +{:8.3}ms  {:?}  paths={:?}", elapsed_ms, ev.kind, paths);
                    count += 1;
                }
                Ok(Err(err)) => println!("  watcher error: {}", err),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        println!("events: {} in {}ms window", count, window.as_millis());
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    // notify on macOS reports paths with /private prefix
    // (canonicalized through the symlink). Use the canonical form
    // ourselves so `strip_prefix` produces clean relative paths.
    let dir_canonical = std::fs::canonicalize(dir.path())?;
    println!("Watching: {} (NonRecursive)", dir_canonical.display());

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(event_tx)?;
    watcher.watch(&dir_canonical, RecursiveMode::NonRecursive)?;

    // FSEvents on macOS replays historical events from when the dir
    // was created; give it a beat to flush them, then drain.
    std::thread::sleep(Duration::from_millis(500));

    let probe = Probe {
        dir: dir_canonical,
        rx: event_rx,
        _watcher: watcher,
    };

    // 1. Create new file
    probe.scenario("Create new file", |dir| {
        std::fs::write(dir.join("a.txt"), "first")?;
        Ok(())
    })?;

    // 2. Modify existing file
    probe.scenario("Modify existing file", |dir| {
        std::fs::write(dir.join("a.txt"), "second")?;
        Ok(())
    })?;

    // 3. Append to existing file
    probe.scenario("Append to existing file", |dir| {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("a.txt"))?;
        f.write_all(b" appended")?;
        f.sync_all()?;
        Ok(())
    })?;

    // 4. Change metadata (permissions only — no content change)
    probe.scenario("Change permissions metadata", |dir| {
        let path = dir.join("a.txt");
        let mut perms = std::fs::metadata(&path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Flip mode to 0o600 so this is a real change even if
            // the existing mode happened to be 0o644.
            perms.set_mode(0o600);
        }
        #[cfg(not(unix))]
        {
            perms.set_readonly(true);
        }
        std::fs::set_permissions(&path, perms)?;
        Ok(())
    })?;

    // 5. Delete file
    probe.scenario("Delete file", |dir| {
        std::fs::remove_file(dir.join("a.txt"))?;
        Ok(())
    })?;

    // 6. Atomic rename swap (write .tmp + rename onto target)
    probe.scenario("Atomic rename swap (write .tmp + rename)", |dir| {
        // Seed a target so the rename overwrites rather than creates.
        std::fs::write(dir.join("b.txt"), "before swap")?;
        // Drain the seed events before the swap itself by waiting a
        // moment; we want the scenario’s timing to reflect just the
        // swap. (This adds a small wait inside the op, which the
        // scenario timer will reflect.)
        std::thread::sleep(Duration::from_millis(100));
        let tmp = dir.join("b.txt.tmp");
        std::fs::write(&tmp, "after swap")?;
        std::fs::rename(&tmp, dir.join("b.txt"))?;
        Ok(())
    })?;

    // 7. Move out of watched dir
    let outside_out = std::env::temp_dir().join(format!("probe-out-{}.txt", std::process::id()));
    probe.scenario("Move out of watched dir", |dir| {
        std::fs::rename(dir.join("b.txt"), &outside_out)?;
        Ok(())
    })?;
    let _ = std::fs::remove_file(&outside_out);

    // 8. Move into watched dir
    let outside_in = std::env::temp_dir().join(format!("probe-in-{}.txt", std::process::id()));
    std::fs::write(&outside_in, "from outside")?;
    probe.scenario("Move into watched dir", |dir| {
        std::fs::rename(&outside_in, dir.join("c.txt"))?;
        Ok(())
    })?;

    // 9. Rapid successive writes
    probe.scenario("Rapid successive writes (5x)", |dir| {
        let path = dir.join("c.txt");
        for i in 0..5 {
            std::fs::write(&path, format!("rapid-{}", i))?;
        }
        Ok(())
    })?;

    println!("\nDone. Temp dir cleaned up on exit.");
    Ok(())
}
