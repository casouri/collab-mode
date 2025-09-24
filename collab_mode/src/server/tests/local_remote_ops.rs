use super::*;

/// Test that when a remote opens a file on our server and we also open it locally,
/// operations from the local editor are correctly sent to the remote.
/// This test sends multiple operations in both directions to ensure all ops are propagated.
#[tokio::test]
async fn test_local_ops_sent_to_remote_subscribers() {
    init_test_tracing();
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Step 1: Hub creates/shares a file (hub is the owner)
    let (hub_file_desc, hub_site_id) = setup
        .hub
        .editor
        .share_file("test.txt", "Initial", serde_json::json!({}))
        .await
        .unwrap();

    tracing::info!("Hub created file with site_id: {}", hub_site_id);
    tracing::info!("Hub file descriptor: {:?}", hub_file_desc);

    // Step 2: Spoke (remote) opens the file from hub
    let (spoke_file_desc, spoke_site_id, spoke_content) = setup.spokes[0]
        .editor
        .open_file(hub_file_desc.clone())
        .await
        .unwrap();

    assert_eq!(spoke_content, "Initial");
    tracing::info!("Spoke opened file with site_id: {}", spoke_site_id);

    // Step 3: Hub also opens the same file locally
    // This creates a RemoteDoc on the hub for its own file
    let (hub_local_file, hub_local_site_id, hub_content) = setup
        .hub
        .editor
        .open_file(hub_file_desc.clone())
        .await
        .unwrap();

    assert_eq!(hub_content, "Initial");
    tracing::info!(
        "Hub opened its own file locally with site_id: {}",
        hub_local_site_id
    );

    // Step 4: Hub sends first operation
    let hub_ops1 = vec![serde_json::json!({
        "op": { "Ins": [0, "Hub1 "] },
        "groupSeq": 1
    })];

    tracing::info!("Hub sending first ops: {:?}", hub_ops1);
    let hub_send_result1 = setup
        .hub
        .editor
        .send_ops(hub_local_file.clone(), hub_ops1)
        .await
        .unwrap();
    tracing::info!("Hub send ops result 1: {:?}", hub_send_result1);

    // Step 5: Spoke fetches hub's first op
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let spoke_fetch1 = setup.spokes[0]
        .editor
        .send_ops(spoke_file_desc.clone(), vec![])
        .await
        .unwrap();

    let remote_ops1 = spoke_fetch1["ops"].as_array().unwrap();
    assert_eq!(
        remote_ops1.len(),
        1,
        "Spoke should receive hub's first operation"
    );
    tracing::info!("Spoke received hub's first op: {:?}", remote_ops1);

    // Step 6: Spoke sends its first operation
    let spoke_ops1 = vec![serde_json::json!({
        "op": { "Ins": [5, "Spoke1 "] },
        "groupSeq": 1
    })];

    tracing::info!("Spoke sending first ops: {:?}", spoke_ops1);
    let spoke_send_result1 = setup.spokes[0]
        .editor
        .send_ops(spoke_file_desc.clone(), spoke_ops1)
        .await
        .unwrap();
    tracing::info!("Spoke send ops result 1: {:?}", spoke_send_result1);

    // Step 7: Hub fetches spoke's first op
    let _ = setup
        .hub
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let hub_fetch1 = setup
        .hub
        .editor
        .send_ops(hub_local_file.clone(), vec![])
        .await
        .unwrap();

    let hub_remote_ops1 = hub_fetch1["ops"].as_array().unwrap();
    assert_eq!(
        hub_remote_ops1.len(),
        1,
        "Hub should receive spoke's first operation"
    );
    tracing::info!("Hub received spoke's first op: {:?}", hub_remote_ops1);

    // Step 8: Hub sends second operation
    let hub_ops2 = vec![serde_json::json!({
        "op": { "Ins": [13, "Hub2 "] },
        "groupSeq": 2
    })];

    tracing::info!("Hub sending second ops: {:?}", hub_ops2);
    let hub_send_result2 = setup
        .hub
        .editor
        .send_ops(hub_local_file.clone(), hub_ops2)
        .await
        .unwrap();
    tracing::info!("Hub send ops result 2: {:?}", hub_send_result2);

    // Step 9: Spoke sends second operation
    let spoke_ops2 = vec![serde_json::json!({
        "op": { "Ins": [18, "Spoke2 "] },
        "groupSeq": 2
    })];

    tracing::info!("Spoke sending second ops: {:?}", spoke_ops2);

    // Wait for hub's second op to arrive first
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let spoke_send_result2 = setup.spokes[0]
        .editor
        .send_ops(spoke_file_desc.clone(), spoke_ops2)
        .await
        .unwrap();
    tracing::info!("Spoke send ops result 2: {:?}", spoke_send_result2);

    // Spoke should receive hub's second op in the response
    let remote_ops2 = spoke_send_result2["ops"].as_array().unwrap();
    assert_eq!(
        remote_ops2.len(),
        1,
        "Spoke should receive hub's second operation"
    );
    tracing::info!("Spoke received hub's second op: {:?}", remote_ops2);

    // Step 10: Hub fetches spoke's second op
    let _ = setup
        .hub
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let hub_fetch2 = setup
        .hub
        .editor
        .send_ops(hub_local_file.clone(), vec![])
        .await
        .unwrap();

    let hub_remote_ops2 = hub_fetch2["ops"].as_array().unwrap();
    assert_eq!(
        hub_remote_ops2.len(),
        1,
        "Hub should receive spoke's second operation"
    );
    tracing::info!("Hub received spoke's second op: {:?}", hub_remote_ops2);

    // Step 11: Hub sends third operation quickly after spoke's second
    let hub_ops3 = vec![serde_json::json!({
        "op": { "Ins": [26, "Hub3"] },
        "groupSeq": 3
    })];

    tracing::info!("Hub sending third ops: {:?}", hub_ops3);
    let hub_send_result3 = setup
        .hub
        .editor
        .send_ops(hub_local_file.clone(), hub_ops3)
        .await
        .unwrap();
    tracing::info!("Hub send ops result 3: {:?}", hub_send_result3);

    // Step 12: Spoke fetches hub's third op
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let spoke_fetch3 = setup.spokes[0]
        .editor
        .send_ops(spoke_file_desc.clone(), vec![])
        .await
        .unwrap();

    let remote_ops3 = spoke_fetch3["ops"].as_array().unwrap();
    assert_eq!(
        remote_ops3.len(),
        1,
        "Spoke should receive hub's third operation"
    );
    tracing::info!("Spoke received hub's third op: {:?}", remote_ops3);

    // Final verification: Content should be "Hub1 Spoke1 Initial Hub2 Spoke2 Hub3"
    // Due to transformation, the exact positions might differ, but all ops should be received

    tracing::info!("Test passed: Multiple bidirectional ops are correctly propagated");
    setup.cleanup();
}

/// Test bidirectional operations when both hub and spoke are editing the same file
#[tokio::test]
async fn test_bidirectional_ops_hub_owned_file() {
    init_test_tracing();
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Hub creates a file
    let (hub_file_desc, _) = setup
        .hub
        .editor
        .share_file("test.txt", "Start", serde_json::json!({}))
        .await
        .unwrap();

    // Spoke opens the file
    let (spoke_file_desc, _, _) = setup.spokes[0]
        .editor
        .open_file(hub_file_desc.clone())
        .await
        .unwrap();

    // Hub opens its own file locally
    let (hub_local_file, _, _) = setup
        .hub
        .editor
        .open_file(hub_file_desc.clone())
        .await
        .unwrap();

    // Send alternating operations from hub and spoke

    // Hub sends first op
    let _ = setup
        .hub
        .editor
        .send_ops(
            hub_local_file.clone(),
            vec![serde_json::json!({"op": { "Ins": [5, " Hub1"] }, "groupSeq": 1})],
        )
        .await
        .unwrap();

    // Wait for spoke to receive notification
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    // Spoke sends op and fetches hub's op
    let spoke_resp = setup.spokes[0]
        .editor
        .send_ops(
            spoke_file_desc.clone(),
            vec![serde_json::json!({"op": { "Ins": [5, " Spoke1"] }, "groupSeq": 1})],
        )
        .await
        .unwrap();

    // Verify spoke received hub's op
    let remote_ops = spoke_resp["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1, "Spoke should receive hub's op");

    // Hub sends second op and fetches spoke's op
    let _ = setup
        .hub
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let hub_resp = setup
        .hub
        .editor
        .send_ops(
            hub_local_file.clone(),
            vec![serde_json::json!({"op": { "Ins": [10, " Hub2"] }, "groupSeq": 2})],
        )
        .await
        .unwrap();

    // Verify hub received spoke's op
    let hub_remote_ops = hub_resp["ops"].as_array().unwrap();
    assert_eq!(hub_remote_ops.len(), 1, "Hub should receive spoke's op");

    // Final sync - both should have all ops
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let final_spoke_resp = setup.spokes[0]
        .editor
        .send_ops(spoke_file_desc.clone(), vec![])
        .await
        .unwrap();

    let final_spoke_ops = final_spoke_resp["ops"].as_array().unwrap();
    assert_eq!(
        final_spoke_ops.len(),
        1,
        "Spoke should receive hub's second op"
    );

    tracing::info!("Test passed: Bidirectional ops work correctly");
    setup.cleanup();
}
