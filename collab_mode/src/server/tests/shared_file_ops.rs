use super::*;

#[tokio::test]
async fn test_server_opens_own_shared_file() {
    // Test: Server can open and send ops to its own file that's already shared with remotes
    // This tests the case where:
    // 1. Hub shares a project with files
    // 2. Spoke connects and opens a file from hub (hub creates Doc for it)
    // 3. Hub then opens the same file itself
    // 4. Hub sends an op to the file
    // Previously this would fail with "File not found" because hub didn't create a RemoteDoc for itself

    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();

    // Create test project with a file and declare on hub
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    tracing::info!("Step 1: Spoke opens file from hub (hub creates Doc)");
    // Spoke requests the file first - this causes hub to create a Doc
    let (spoke_file, spoke_site_id, spoke_content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(spoke_content, "Hello from test.txt");
    tracing::info!("Spoke opened file with site_id: {}", spoke_site_id);

    tracing::info!("Step 2: Hub opens the same file itself");
    // Now hub opens the same file itself
    // This should find the existing Doc and also create a RemoteDoc for hub
    let (hub_file, hub_site_id, hub_content) = setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(hub_content, "Hello from test.txt");
    assert_eq!(hub_site_id, 0, "Hub should have site_id 0");
    tracing::info!("Hub opened file with site_id: {}", hub_site_id);

    tracing::info!("Step 3: Hub sends an op to its own file");
    // Hub sends an op to the file
    // This should work because hub has a RemoteDoc for itself
    let ops = vec![serde_json::json!({
        "op": { "kind": "Ins", "pos": 0, "content": "Hub says: " },
        "groupSeq": 1
    })];

    let send_ops_result = setup.hub.editor.send_ops(hub_file.clone(), ops).await;

    assert!(
        send_ops_result.is_ok(),
        "Hub should be able to send ops to its own file: {:?}",
        send_ops_result.err()
    );

    let send_ops_resp = send_ops_result.unwrap();
    tracing::info!("Hub successfully sent op, response: {:?}", send_ops_resp);

    // The last_global_seq might be 0 initially since the server processes ops asynchronously
    // What's important is that the op was sent without error
    let _last_global_seq = send_ops_resp.last_global_seq;

    tracing::info!("Step 4: Verify spoke receives the op");
    // Spoke should receive RemoteOpsArrived notification
    let _notification = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    // Spoke fetches the remote op
    let fetch_ops_resp = setup.spokes[0]
        .editor
        .send_ops(spoke_file.clone(), vec![])
        .await
        .unwrap();

    assert_eq!(
        fetch_ops_resp.ops.len(),
        1,
        "Spoke should receive 1 op from hub"
    );

    let op = &fetch_ops_resp.ops[0];
    // Verify it's an insert operation from the hub
    assert_eq!(op.site_id, hub_site_id, "Op should be from hub");
    let expected_edit_inst = crate::types::EditInstruction::Ins {
        edits: vec![crate::types::Edit {
            pos: 0,
            content: "Hub says: ".to_string(),
        }],
    };
    assert_eq!(
        &op.op, &expected_edit_inst,
        "Spoke should receive the correct op from hub"
    );

    tracing::info!("Test completed successfully - hub can send ops to its own shared file");

    setup.cleanup();
}

#[tokio::test]
async fn test_server_opens_own_file_before_remote() {
    // Test the opposite case: Hub opens file first, then spoke opens it
    // This should already work, but good to verify

    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();

    // Create test project with a file and declare on hub
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    tracing::info!("Step 1: Hub opens file first");
    // Hub opens the file first (creates both Doc and RemoteDoc)
    let (hub_file, hub_site_id, hub_content) = setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(hub_content, "Hello from test.txt");
    assert_eq!(hub_site_id, 0, "Hub should have site_id 0");

    tracing::info!("Step 2: Spoke opens the same file");
    // Spoke requests the file
    let (spoke_file, spoke_site_id, spoke_content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(spoke_content, "Hello from test.txt");
    assert!(spoke_site_id > 0, "Spoke should have non-zero site_id");

    tracing::info!("Step 3: Hub sends an op");
    // Hub sends an op - this should work fine
    let ops = vec![serde_json::json!({
        "op": { "kind": "Ins", "pos": 0, "content": "Working: " },
        "groupSeq": 1
    })];

    let send_ops_resp = setup
        .hub
        .editor
        .send_ops(hub_file.clone(), ops)
        .await
        .unwrap();

    // The last_global_seq might be 0 initially since the server processes ops asynchronously
    let _last_global_seq = send_ops_resp.last_global_seq;

    tracing::info!("Step 4: Verify spoke receives the op");
    // Spoke should receive the op
    let _notification = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let fetch_ops_resp = setup.spokes[0]
        .editor
        .send_ops(spoke_file.clone(), vec![])
        .await
        .unwrap();

    assert_eq!(
        fetch_ops_resp.ops.len(),
        1,
        "Spoke should receive 1 op from hub"
    );

    tracing::info!("Test completed successfully - normal case works as expected");

    setup.cleanup();
}

#[tokio::test]
async fn test_move_file_permission_denied() {
    let factory = TestChannelFactory::new();

    // Set up hub and spoke with write permission denied.
    let mut setup = setup_hub_and_spoke_servers(
        &factory,
        1,
        Some(vec![Permission {
            write: false,
            create: true,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    // Declare a project on the hub.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "test-project")
        .await
        .unwrap();

    // Try to move a file from spoke (should be denied).
    let result = setup.spokes[0]
        .editor
        .request(
            "MoveFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "project": "test-project",
                "oldPath": "test.txt",
                "newPath": "renamed.txt",
            }),
        )
        .await;

    assert!(result.is_err());
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            error_str.contains("Permission")
                || error_str.contains("permission")
                || error_str.contains("denied"),
            "Expected permission denied error, got: {}",
            error_str
        );
    }

    setup.cleanup();
}

#[tokio::test]
async fn test_save_file_permission_denied() {
    let factory = TestChannelFactory::new();

    // Set up hub and spoke with write permission denied.
    let mut setup = setup_hub_and_spoke_servers(
        &factory,
        1,
        Some(vec![Permission {
            write: false,
            create: true,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    // Declare a project on the hub.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "test-project")
        .await
        .unwrap();

    // Open the file from spoke first (this should succeed since it's read-only open).
    let file_desc = serde_json::json!({
        "hostId": setup.hub.id.clone(),
        "project": "test-project",
        "file": "test.txt"
    });
    let _open_result = setup.spokes[0]
        .editor
        .request(
            "OpenFile",
            serde_json::json!({
                "file": file_desc.clone(),
                "mode": "open",
            }),
        )
        .await
        .unwrap();

    // Try to save the file from spoke (should be denied).
    let result = setup.spokes[0]
        .editor
        .request(
            "SaveFile",
            serde_json::json!({
                "file": file_desc,
            }),
        )
        .await;

    assert!(result.is_err());
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            error_str.contains("Permission")
                || error_str.contains("permission")
                || error_str.contains("denied"),
            "Expected permission denied error, got: {}",
            error_str
        );
    }

    setup.cleanup();
}
