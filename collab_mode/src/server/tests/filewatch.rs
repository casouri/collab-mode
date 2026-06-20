use super::*;
use crate::filewatch_receptor;
use crate::server::transcript_tests::MockDocument;
use std::sync::Arc;
use tokio::sync::Notify;

/// Move the hub’s filewatch channels out of the setup and spawn a
/// real `filewatch_receptor::run` on a system thread, wired to the
/// hub’s `Server`. After this, opening a local file on the hub causes
/// the server to send `WatchFiles` to the real receptor, which
/// configures `notify` to watch the file’s parent directory.
/// Filesystem changes then flow back through the real receptor.
///
/// The returned `Notify` can be used to ask the receptor to shut
/// down, but it isn’t strictly required, because when
/// `setup.cleanup()` aborts the hub server, the receptor’s channels
/// close and it exits on its own.
fn spawn_hub_filewatch_receptor(setup: &mut HubAndSpokeSetup) -> Arc<Notify> {
    // tx (receptor → server): tokio mpsc.
    let (dummy_send_tx, _dummy_send_rx) = mpsc::channel(1);
    // rx (server → receptor): crossbeam.
    let (_dummy_recv_tx, dummy_recv_rx) = crossbeam_channel::bounded(1);
    let tx = std::mem::replace(&mut setup.hub_filewatch_to_server_tx, dummy_send_tx);
    let rx = std::mem::replace(&mut setup.hub_server_to_filewatch_rx, dummy_recv_rx);
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_run = shutdown.clone();
    std::thread::spawn(move || {
        if let Err(err) = filewatch_receptor::run(tx, rx, shutdown_for_run) {
            tracing::warn!("test filewatch receptor exited: {}", err);
        }
    });
    shutdown
}

/// Time to give notify’s OS layer to register a newly-watched
/// directory before a filesystem change is reliably observed.
const NOTIFY_REGISTER_WAIT: Duration = Duration::from_millis(500);
/// Time to wait for a RemoteOpsArrived to arrive (or not).
const PROPAGATE_WAIT_SECS: u64 = 5;
/// Negative-case window: how long to wait before concluding a
/// notification will not arrive.
const NEGATIVE_WAIT: Duration = Duration::from_millis(800);

// All three tests are filesystem-event driven and flaky on macOS
// FSEvents under parallel `cargo test` load (multiple watchers
// compete and FSEvents drops events). They pass reliably when run
// individually:
//   cargo test --lib -- --ignored server::tests::filewatch

#[ignore]
#[tokio::test]
async fn external_change_broadcasts_to_subscribers() {
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();
    let _receptor_shutdown = spawn_hub_filewatch_receptor(&mut setup);

    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    // Hub opens the file; this seeds the in-memory buffer AND tells
    // the receptor (via WatchFiles) to start watching its parent dir.
    setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    // Spoke opens it too so we have a remote subscriber. Seed a mock
    // doc with the initial content so we can replay the ops the spoke
    // receives and compare against disk.
    let (spoke_file, _, initial_content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    let mut mock_doc = MockDocument::new(&initial_content);

    // Give notify a beat to bind to the watched directory.
    tokio::time::sleep(NOTIFY_REGISTER_WAIT).await;

    // External program rewrites the file.
    let on_disk = project_dir.path().join("test.txt");
    let new_content = "Formatted: Hello from test.txt";
    std::fs::write(&on_disk, new_content).unwrap();

    // Spoke should be notified once the real receptor delivers the
    // event and the hub diffs+broadcasts.
    setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", PROPAGATE_WAIT_SECS)
        .await
        .unwrap();
    let fetch_resp = setup.spokes[0]
        .editor
        .send_ops(spoke_file, vec![])
        .await
        .unwrap();
    assert_eq!(fetch_resp.doc_len, new_content.chars().count() as u64);

    // Replay the lean ops the spoke received and confirm the resulting
    // buffer is byte-for-byte the new on-disk content.
    for op in &fetch_resp.ops {
        mock_doc
            .apply_edit_instruction(&op.op)
            .expect("applying remote op");
    }
    assert_eq!(mock_doc.get_content(), new_content);

    setup.cleanup();
}

#[ignore]
#[tokio::test]
async fn rewrite_with_same_content_no_broadcast() {
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();
    let _receptor_shutdown = spawn_hub_filewatch_receptor(&mut setup);

    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();
    setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();

    tokio::time::sleep(NOTIFY_REGISTER_WAIT).await;

    // Rewrite with the same content. notify will fire, receptor will
    // forward, hub will diff and find zero ops, and not broadcast.
    let on_disk = project_dir.path().join("test.txt");
    std::fs::write(&on_disk, "Hello from test.txt").unwrap();

    // Wait briefly and assert no RemoteOpsArrived.
    let got = tokio::time::timeout(
        NEGATIVE_WAIT,
        setup.spokes[0]
            .editor
            .wait_for_notification("RemoteOpsArrived", 1),
    )
    .await;
    match got {
        Err(_) => {}     // outer timeout: nothing arrived
        Ok(Err(_)) => {} // inner wait_for_notification timed out
        Ok(Ok(v)) => panic!("expected no RemoteOpsArrived; got {:?}", v),
    }

    setup.cleanup();
}

#[ignore]
#[tokio::test]
async fn close_stops_propagation() {
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();
    let _receptor_shutdown = spawn_hub_filewatch_receptor(&mut setup);

    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();
    setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    let (spoke_file, _, initial_content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    let mut mock_doc = MockDocument::new(&initial_content);

    tokio::time::sleep(NOTIFY_REGISTER_WAIT).await;

    // Sanity: the wiring works while the file is open.
    let on_disk = project_dir.path().join("test.txt");
    let first_change = "first change";
    std::fs::write(&on_disk, first_change).unwrap();
    setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", PROPAGATE_WAIT_SECS)
        .await
        .unwrap();
    // Drain the ops so subsequent waits don’t see a stale message, and
    // confirm the replayed buffer matches disk.
    let fetch_resp = setup.spokes[0]
        .editor
        .send_ops(spoke_file.clone(), vec![])
        .await
        .unwrap();
    for op in &fetch_resp.ops {
        mock_doc
            .apply_edit_instruction(&op.op)
            .expect("applying remote op");
    }
    assert_eq!(mock_doc.get_content(), first_change);

    // Close the file on the hub. The spoke also receives a FileClosed
    // notification as part of close propagation; consume it.
    setup
        .hub
        .editor
        .request(
            "CloseFile",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();
    // Best effort: drain the FileClosed notification if it arrives.
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        setup.spokes[0]
            .editor
            .wait_for_notification("FileClosed", 1),
    )
    .await;

    // External program modifies the file again after close. With the
    // doc removed from the hub and the watcher unwatched, no
    // RemoteOpsArrived should reach the spoke.
    std::fs::write(&on_disk, "second change after close").unwrap();

    let got = tokio::time::timeout(
        NEGATIVE_WAIT,
        setup.spokes[0]
            .editor
            .wait_for_notification("RemoteOpsArrived", 1),
    )
    .await;
    match got {
        Err(_) => {}
        Ok(Err(_)) => {}
        Ok(Ok(v)) => panic!("expected no RemoteOpsArrived after close; got {:?}", v),
    }

    setup.cleanup();
}
