use super::*;

#[tokio::test]
async fn test_doc_project_handling() {
    let mut env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&mut env, 1, None)
        .await
        .unwrap();

    // Test 1: Attempt to declare a project named "_buffers" should be rejected.
    let resp = setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [
                    {
                        "name": "_buffers",
                        "path": "/tmp/test",
                    }
                ]
            }),
        )
        .await;
    assert!(resp.is_err());
    if let Err(err) = resp {
        let error_str = err.to_string();
        assert!(error_str.contains("reserved"));
    }

    // Test 2: Create a shared file and access it via _buffers project.
    let share_resp = setup
        .hub
        .editor
        .request(
            "ShareFile",
            serde_json::json!({
                "filename": "test_shared.txt",
                "content": "This is a shared document",
                "meta": {}
            }),
        )
        .await
        .unwrap();
    let shared_file = share_resp["file"].clone();

    // Test 3: Open the shared file using _buffers project.
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "hostId": setup.hub.id.clone(),
                    "project": "_buffers",
                    "file": "test_shared.txt",
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let open_resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    assert_eq!(
        open_resp["content"].as_str().unwrap(),
        "This is a shared document"
    );
    assert_eq!(open_resp["filename"].as_str().unwrap(), "test_shared.txt");
    assert_eq!(open_resp["file"], shared_file);

    // Test 4: List files in _buffers project.
    let req_id = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "_buffers",
                    "file": ""
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();
    let list_resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let files = list_resp["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["filename"].as_str().unwrap(), "test_shared.txt");
    assert_eq!(files[0]["isDirectory"].as_bool().unwrap(), false);

    // Test 5: Try to open a non-existent file in _doc.
    let req_id = 3;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "hostId": setup.hub.id.clone(),
                    "project": "_doc",
                    "file": "nonexistent.txt",
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let error_resp = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(error_resp.is_err() || error_resp.as_ref().unwrap().get("error").is_some());

    setup.cleanup();
}
