use super::*;

#[tokio::test]
async fn test_delete_file() {
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Create a test project with some files and directories.
    let test_dir = tempfile::tempdir().unwrap();
    let project_path = test_dir.path().to_path_buf();
    std::fs::write(project_path.join("file1.txt"), "Content 1").unwrap();
    std::fs::create_dir(project_path.join("dir1")).unwrap();
    std::fs::write(project_path.join("dir1/file2.txt"), "Content 2").unwrap();
    std::fs::write(project_path.join("dir1/file3.txt"), "Content 3").unwrap();

    // Declare the project.
    setup
        .hub
        .editor
        .declare_project(&project_path.display().to_string(), "TestProject")
        .await
        .unwrap();

    // Give time for project declaration to propagate
    sleep(Duration::from_millis(100)).await;

    // Spoke 0 needs to open files to get subscribed to them
    // Open file1.txt
    let req_id = 41;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "file1.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let _resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();

    // Also open a file from dir1 to get subscribed to directory changes
    let req_id = 42;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "dir1/file2.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let _resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();

    // Wait a bit for subscriptions to be established
    sleep(Duration::from_millis(200)).await;

    // Delete file1.txt.
    let req_id = 5;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "DeleteFile",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "file1.txt"
                }
            }),
        )
        .await
        .unwrap();
    setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Spoke 0 should receive FileDeleted notification for the file.
    let notification = setup.spokes[0]
        .editor
        .wait_for_notification("FileDeleted", 5)
        .await
        .unwrap();
    assert!(!notification["file"]["file"].as_str().unwrap().is_empty());
    assert_eq!(
        notification["file"]["project"].as_str().unwrap(),
        "TestProject"
    );
    assert_eq!(notification["file"]["file"].as_str().unwrap(), "file1.txt");
    assert!(!project_path.join("file1.txt").exists());

    // Test 2: Delete a directory (dir1).
    let req_id = 8;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "DeleteFile",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "dir1"
                }
            }),
        )
        .await
        .unwrap();
    setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Spoke 0 should receive FileDeleted for the directory.
    let notification = setup.spokes[0]
        .editor
        .wait_for_notification("FileDeleted", 5)
        .await
        .unwrap();
    assert!(!notification["file"]["file"].as_str().unwrap().is_empty());
    assert_eq!(
        notification["file"]["project"].as_str().unwrap(),
        "TestProject"
    );
    assert_eq!(notification["file"]["file"].as_str().unwrap(), "dir1");
    assert!(!project_path.join("dir1").exists());

    setup.cleanup();
}

#[tokio::test]
async fn test_delete_file_permission_denied() {
    let env = TestEnvironment::new().await.unwrap();

    // Set up hub and spoke with delete permission denied.
    let mut setup = setup_hub_and_spoke_servers(
        &env,
        1,
        Some(vec![Permission {
            write: true,
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
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path,
                    "name": "test-project",
                }]
            }),
        )
        .await
        .unwrap();

    // First, share a file on the hub.
    let (_doc_id, _site_id) = setup
        .hub
        .editor
        .share_file("test.txt", "test content", serde_json::json!({}))
        .await
        .unwrap();

    // Try to delete the file from spoke (should be denied).
    let result = setup.spokes[0]
        .editor
        .request(
            "DeleteFile",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "test-project",
                    "file": "test.txt",
                },
            }),
        )
        .await;

    assert!(result.is_err());
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            error_str.contains("Permission")
                || error_str.contains("permission")
                || error_str.contains("denied")
        );
    }
}
