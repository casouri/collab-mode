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

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

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
        "op": { "Ins": [0, "Hub says: "] },
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

    // The lastSeq might be 0 initially since the server processes ops asynchronously
    // What's important is that the op was sent without error
    let _last_seq = send_ops_resp["lastSeq"].as_u64().unwrap();

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

    let remote_ops = fetch_ops_resp["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1, "Spoke should receive 1 op from hub");

    let op = &remote_ops[0];
    let expected_op = serde_json::json!({
        "op": { "Ins": [[0, "Hub says: "]] },
        "siteId": hub_site_id,
    });
    assert_eq!(
        serde_json::to_string(op).unwrap(),
        serde_json::to_string(&expected_op).unwrap(),
        "Spoke should receive the correct op from hub"
    );

    tracing::info!("Test completed successfully - hub can send ops to its own shared file");

    setup.cleanup();
}

#[tokio::test]
async fn test_server_opens_own_file_before_remote() {
    // Test the opposite case: Hub opens file first, then spoke opens it
    // This should already work, but good to verify

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

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
        "op": { "Ins": [0, "Working: "] },
        "groupSeq": 1
    })];

    let send_ops_resp = setup
        .hub
        .editor
        .send_ops(hub_file.clone(), ops)
        .await
        .unwrap();

    // The lastSeq might be 0 initially since the server processes ops asynchronously
    let _last_seq = send_ops_resp["lastSeq"].as_u64().unwrap();

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

    let remote_ops = fetch_ops_resp["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1, "Spoke should receive 1 op from hub");

    tracing::info!("Test completed successfully - normal case works as expected");

    setup.cleanup();
}
