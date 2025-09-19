use super::*;

#[tokio::test]
async fn test_send_info_local_file() {
    // Send info for a local file; spoke should receive InfoReceived.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Share a local file on hub.
    let (hub_file, _site_id) = setup
        .hub
        .editor
        .share_file(
            "test_info.txt",
            "Content for info test",
            serde_json::json!({}),
        )
        .await
        .unwrap();

    // Spoke opens the remote file.
    let spoke_file_desc = serde_json::json!({
        "hostId": setup.hub.id.clone(),
        "project": "_buffers",
        "file": "test_info.txt"
    });

    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": spoke_file_desc.clone(),
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let _ = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();

    // Wait for subscription to be established.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Hub sends info for the local file.
    let info_data = serde_json::json!({
        "cursor": 10,
        "selection": [5, 10],
        "user": "hub-editor"
    });
    setup
        .hub
        .editor
        .send_info(hub_file.clone(), info_data.clone())
        .await
        .unwrap();

    let notification = setup.spokes[0]
        .editor
        .wait_for_notification("InfoReceived", 5)
        .await
        .unwrap();
    assert_eq!(notification["file"], spoke_file_desc);

    let info_str = notification["info"].as_str().unwrap();
    let info_value: serde_json::Value = serde_json::from_str(info_str).unwrap();
    assert_eq!(info_value, info_data);

    setup.cleanup();
}

#[tokio::test]
async fn test_send_info_remote_file() {
    // Spoke sends info for a remote file; hub/subscribers should receive it.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Hub shares a file.
    let (_hub_file, _site_id) = setup
        .hub
        .editor
        .share_file(
            "remote_info_test.txt",
            "Content for remote info",
            serde_json::json!({}),
        )
        .await
        .unwrap();

    // Spoke 1 opens the remote file.
    let spoke1_file_desc = serde_json::json!({
        "hostId": setup.hub.id.clone(),
        "project": "_buffers",
        "file": "remote_info_test.txt"
    });
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": spoke1_file_desc.clone(),
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let _ = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();

    // Spoke 2 also opens the same file.
    let spoke2_file_desc = spoke1_file_desc.clone();
    let req_id = 2;
    setup.spokes[1]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": spoke2_file_desc.clone(),
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let _ = setup.spokes[1]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Spoke 1 sends info for the remote file.
    let info_data = serde_json::json!({
        "cursor": 15,
        "selection": [10, 20],
        "user": "spoke1-editor"
    });
    setup.spokes[0]
        .editor
        .send_info(spoke1_file_desc.clone(), info_data.clone())
        .await
        .unwrap();

    // Spoke 2 should receive InfoReceived.
    let notification = setup.spokes[1]
        .editor
        .wait_for_notification("InfoReceived", 5)
        .await
        .unwrap();
    assert_eq!(notification["file"], spoke2_file_desc);

    let info_str = notification["info"].as_str().unwrap();
    let info_value: serde_json::Value = serde_json::from_str(info_str).unwrap();
    assert_eq!(info_value, info_data);

    setup.cleanup();
}

#[tokio::test]
async fn test_send_info_multiple_subscribers() {
    // Hub sends info; all spokes subscribed to the file should receive it.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 3, None).await.unwrap();

    // Hub shares a file.
    let (hub_file, _site_id) = setup
        .hub
        .editor
        .share_file(
            "multi_subscriber_test.txt",
            "Content for multiple subscribers",
            serde_json::json!({}),
        )
        .await
        .unwrap();

    // All spokes open the file.
    let mut spoke_files = vec![];
    for (i, spoke) in setup.spokes.iter_mut().enumerate() {
        let spoke_file_desc = serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "_buffers",
            "file": "multi_subscriber_test.txt"
        });

        let req_id = (i + 1) as i32;
        spoke
            .editor
            .send_request(
                req_id,
                "OpenFile",
                serde_json::json!({
                    "fileDesc": spoke_file_desc.clone(),
                    "mode": "open"
                }),
            )
            .await
            .unwrap();
        let _ = spoke.editor.wait_for_response(req_id, 5).await.unwrap();
        spoke_files.push(spoke_file_desc);
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Hub sends info.
    let info_data = serde_json::json!({
        "status": "editing",
        "line": 5,
        "column": 10
    });
    setup
        .hub
        .editor
        .send_info(hub_file.clone(), info_data.clone())
        .await
        .unwrap();

    // All spokes should receive the info.
    for (i, spoke) in setup.spokes.iter_mut().enumerate() {
        let notification = spoke
            .editor
            .wait_for_notification("InfoReceived", 5)
            .await
            .unwrap();

        assert_eq!(notification["file"], spoke_files[i]);

        let info_str = notification["info"].as_str().unwrap();
        let info_value: serde_json::Value = serde_json::from_str(info_str).unwrap();
        assert_eq!(info_value, info_data);
    }

    setup.cleanup();
}
