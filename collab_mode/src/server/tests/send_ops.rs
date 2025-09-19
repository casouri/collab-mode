use super::*;

#[tokio::test]
async fn test_send_ops_e2e() {
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Create test project with a file and declare on hub.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    // Hub opens the file first (to create it in both docs and remote_docs).
    let (hub_file, _hub_site_id, _hub_content) = setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();

    // Spoke 1 requests the file.
    let (spoke1_file, _spoke1_site_id, spoke1_content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(spoke1_content, "Hello from test.txt");

    // Spoke 2 requests the file.
    let (spoke2_file, _spoke2_site_id, spoke2_content) = setup.spokes[1]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(spoke2_content, "Hello from test.txt");

    // Step 1: Hub sends op.
    let ops = vec![serde_json::json!({
        "op": { "Ins": [0, "Modified: "] },
        "groupSeq": 1
    })];
    let _ = setup.hub.editor.send_ops(hub_file.clone(), ops).await.unwrap();

    // Spoke 2 should receive RemoteOpsArrived and then fetch remote ops.
    let _notification = setup.spokes[1]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let fetch_ops_resp = setup.spokes[1]
        .editor
        .send_ops(spoke2_file.clone(), vec![])
        .await
        .unwrap();
    let remote_ops = fetch_ops_resp["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1);

    let op = &remote_ops[0];
    let expected_op = serde_json::json!({
        "op": { "Ins": [[0, "Modified: "]] },
        "siteId": 0,
    });
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    // Spoke 1 also receives and fetches the op.
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let fetch_ops_resp_spoke1 = setup.spokes[0]
        .editor
        .send_ops(spoke1_file.clone(), vec![])
        .await
        .unwrap();
    let remote_ops = fetch_ops_resp_spoke1["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1);
    let op = &remote_ops[0];
    let expected_op = serde_json::json!({
        "op": { "Ins": [[0, "Modified: "]] },
        "siteId": 0,
    });
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    // Step 2: Spoke 1 sends an op.
    let _ = setup.spokes[0]
        .editor
        .send_ops(
            spoke1_file.clone(),
            vec![serde_json::json!({
                "op": { "Ins": [10, "[Spoke1]"] },
                "groupSeq": 1
            })],
        )
        .await
        .unwrap();

    // Step 3: Spoke 2 sends an op after receiving Spoke 1’s op; response should include buffered op.
    let _ = setup.spokes[1]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();
    let send_ops_resp = setup.spokes[1]
        .editor
        .send_ops(
            spoke2_file.clone(),
            vec![serde_json::json!({
                "op": { "Ins": [10, "[Spoke2]"] },
                "groupSeq": 1
            })],
        )
        .await
        .unwrap();
    let remote_ops = send_ops_resp["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1);

    // Spoke 1 later fetches Spoke 2's op (transformed position).
    // Wait for notification that remote ops have arrived
    let _ = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    let ops_from_spoke2 = setup.spokes[0]
        .editor
        .send_ops(spoke1_file.clone(), vec![])
        .await
        .unwrap()["ops"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(ops_from_spoke2.len(), 1);

    setup.cleanup();
}

#[tokio::test]
async fn test_send_ops_permission_denied() {
    let env = TestEnvironment::new().await.unwrap();

    // Set up hub and spoke with write permission denied.
    let mut setup = setup_hub_and_spoke_servers(
        &env,
        1,
        Some(vec![Permission {
            write: false,
            create: true,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    // Share a file on the hub.
    let (file_desc, _site_id) = setup
        .hub
        .editor
        .share_file("test.txt", "initial content", serde_json::json!({}))
        .await
        .unwrap();

    // Spoke opens the file first.
    let _open_result = setup.spokes[0]
        .editor
        .request(
            "OpenFile",
            serde_json::json!({
                "fileDesc": file_desc,
                "mode": "open",
            }),
        )
        .await
        .unwrap();

    // Try to send ops from spoke (should be denied on hub side; local request still OK).
    let result = setup.spokes[0]
        .editor
        .send_ops(
            file_desc.clone(),
            vec![serde_json::json!({
                "op": {"Ins": [0, "new text"]},
                "groupSeq": 0,
            })],
        )
        .await;

    assert!(result.is_ok(), "SendOps should succeed locally");

    // Wait for InternalError notification about permission denial.
    let notification = setup.spokes[0]
        .editor
        .wait_for_notification(&NotificationCode::InternalError.to_string(), 5)
        .await;
    assert!(notification.is_ok());

    if let Ok(params) = notification {
        let message = params
            .as_object()
            .and_then(|obj| obj.get("message"))
            .and_then(|msg| msg.as_str())
            .unwrap_or("");
        assert!(message.contains("denied"));
    }
}

