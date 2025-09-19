use super::*;

#[tokio::test]
async fn test_open_file_basic() {
    // Test: Basic file opening from local editor.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project.
    let project_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(project_dir.path().join("test.txt"), "Hello from test.txt").unwrap();
    std::fs::create_dir(project_dir.path().join("src")).unwrap();
    std::fs::write(
        project_dir.path().join("src/main.rs"),
        "fn main() {\n    println!(\"Hello\");\n}",
    )
    .unwrap();
    std::fs::write(
        project_dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }",
    )
    .unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Declare the project on the hub so the request is valid at the project level.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // Request to open a file.
    let (file_desc, site_id, content) = setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();

    // Verify response.
    assert_eq!(content, "Hello from test.txt");
    assert!(file_desc.is_object());
    assert!(site_id >= 0);

    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_from_remote() {
    // Test: Remote server requests file from hub.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create test project.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke requests to open a file from hub.
    let (_file_desc, site_id, content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "src/main.rs"
        }))
        .await
        .unwrap();

    // Verify response.
    assert_eq!(content, "fn main() {\n    println!(\"Hello\");\n}");
    assert!(site_id > 0);

    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_create_mode() {
    // Test: Create a new file using OpenMode::Create.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request to create a new file.
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
                    "project": "TestProject",
                    "file": "new_file.txt"
                },
                "mode": "create"
            }),
        )
        .await
        .unwrap();

    // Should succeed and return empty content.
    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let file1 = resp["file"].clone();
    let content = resp["content"].as_str().unwrap();
    assert_eq!(content, "");
    assert!(file1.is_object());

    // Send an insert operation to add content.
    let ops = vec![serde_json::json!({
        "op": {"Ins": [0, "This is a newly created file!"]},
        "groupSeq": 1
    })];
    let _ = setup.hub.editor.send_ops(file1.clone(), ops).await.unwrap();

    sleep(Duration::from_millis(100)).await;

    // Open the file again with "open" mode to verify it exists and has content.
    let req_id2 = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id2,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "new_file.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let resp2 = setup.hub.editor.wait_for_response(req_id2, 5).await.unwrap();
    let file2 = resp2["file"].clone();
    assert_eq!(file1, file2);

    // Verify the file was created on disk.
    let file_path = project_dir.path().join("new_file.txt");
    assert!(file_path.exists());

    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_not_found() {
    // Test: Handle file not found error.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // Request to open a non-existent file.
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
                    "project": "TestProject",
                    "file": "non_existent.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    // Wait for error response.
    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Failed to") || err_msg.contains("not found") || err_msg.contains("105")
    );

    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_bad_request() {
    // Test: Handle bad request (trying to open a directory).
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Request to open a project directory (file is empty).
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
                    "project": "TestProject",
                    "file": ""
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    // Wait for error response.
    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("113") || err_msg.contains("Cannot open a project directory"));

    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_already_open() {
    // Test: Handle already opened file.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // First request to open a file.
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
                    "project": "TestProject",
                    "file": "test.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let resp1 = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Second request to open the same file.
    let req_id = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "test.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();
    let resp2 = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    assert_eq!(resp1["file"], resp2["file"]);
    assert_eq!(resp2["content"], "Hello from test.txt");

    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_doc_id_not_found() {
    // Test: Handle request for non-existent doc by ID via _files.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    let req_id = 1;
    let temp_dir = tempfile::TempDir::new().unwrap();
    let missing_path = temp_dir
        .path()
        .join("nonexistent.txt")
        .to_string_lossy()
        .to_string();

    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "hostId": setup.hub.id.clone(),
                    "project": "_files",
                    "file": missing_path
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("not found") || err_msg.contains("Failed to"));

    setup.cleanup();
}

#[tokio::test]
async fn test_open_binary_file_rejected() {
    let mut env = TestEnvironment::new().await.unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();

    // Create a binary file.
    let binary_file_path = project_dir.path().join("test.bin");
    let binary_content: Vec<u8> = vec![0x00, 0xFF, 0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x42];
    std::fs::write(&binary_file_path, binary_content).unwrap();

    let project_path = project_dir.path().to_string_lossy().to_string();

    let mut setup = setup_hub_and_spoke_servers(&mut env, 1, None)
        .await
        .unwrap();

    // Declare project on hub.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    // Try to open the binary file.
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
                    "project": "TestProject",
                    "file": "test.bin",
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let resp = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(resp.is_err() || resp.as_ref().unwrap().get("error").is_some());

    if let Ok(response) = resp {
        if let Some(error) = response.get("error") {
            let error_message = error.get("message").unwrap().as_str().unwrap();
            assert!(error_message.contains("Cannot open binary file"));
        }
    } else if let Err(err) = resp {
        let error_str = err.to_string();
        assert!(error_str.contains("Cannot open binary file"));
    }

    setup.cleanup();
}

