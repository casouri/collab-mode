use super::*;

#[tokio::test]
async fn test_list_files_project_directory() {
    // List files in a specific project directory.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    let project_dir = super::create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "Project2")
        .await
        .unwrap();

    // Spoke requests listing of src directory.
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "Project2",
                    "file": "src"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0].editor.wait_for_response(req_id, 5).await.unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should have main.rs, lib.rs, modules.
    assert_eq!(files.len(), 3);

    // Request listing of src/modules directory.
    let req_id = 2;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "Project2",
                    "file": "src/modules"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0].editor.wait_for_response(req_id, 5).await.unwrap();
    let files = resp["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);

    // Verify nested file paths.
    let mod1 = files
        .iter()
        .find(|f| f["filename"].as_str() == Some("mod1.rs"))
        .unwrap();
    assert_eq!(mod1["file"]["file"].as_str(), Some("src/modules/mod1.rs"));
}

#[tokio::test]
async fn test_list_files_from_remote() {
    // Spoke requests listing from hub.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Create project and declare on hub.
    let project_dir = super::create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    setup
        .hub
        .editor
        .declare_project(&project_path, "Project2")
        .await
        .unwrap();

    // Spoke 1 lists files.
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "Project2",
                    "file": "src/modules"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();
    let resp = setup.spokes[0].editor.wait_for_response(req_id, 5).await.unwrap();
    let files = resp["files"].as_array().unwrap();

    // Spoke 2 lists files.
    let req_id2 = 2;
    setup.spokes[1]
        .editor
        .send_request(
            req_id2,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "Project2",
                    "file": "src/modules"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();
    let resp2 = setup.spokes[1].editor.wait_for_response(req_id2, 5).await.unwrap();
    let files2 = resp2["files"].as_array().unwrap();

    assert_eq!(files.len(), files2.len());
}

#[tokio::test]
async fn test_list_files_project_not_found() {
    // Request listing with invalid project ID.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "/non/existent/project",
                    "file": "src"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let result = setup.spokes[0].editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("500") || err_msg.contains("Project") || err_msg.contains("not found")
    );
}

#[tokio::test]
async fn test_list_files_not_directory() {
    // Request listing of a file path (not directory).
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    // Request listing of a file (not directory).
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "test.txt"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let result = setup.spokes[0].editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("Not a directory") || err_msg.contains("500"));
}

#[tokio::test]
async fn test_list_files_empty_directory() {
    // List an empty directory.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    let project_dir = super::create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    // Request listing of empty directory.
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "empty"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();
    assert!(files.is_empty());
}

#[tokio::test]
async fn test_list_files_nested_structure() {
    // Verify list of nested structure.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    let project_dir = super::create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "src/modules"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();
    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);

    let mod1 = files
        .iter()
        .find(|f| f["filename"].as_str() == Some("mod1.rs"))
        .unwrap();
    assert_eq!(mod1["file"]["file"].as_str(), Some("src/modules/mod1.rs"));
}

#[tokio::test]
async fn test_list_files_sorted_alphanumerically() {
    let mut env = TestEnvironment::new().await.unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();

    // Files intentionally out of order.
    std::fs::write(project_dir.path().join("zebra.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("apple.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("banana.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("1_first.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("10_tenth.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("2_second.txt"), "content").unwrap();

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

    // List files in the project directory.
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": ""
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    let files = resp["files"].as_array().unwrap();
    assert_eq!(files.len(), 6);

    let filenames: Vec<String> = files
        .iter()
        .map(|f| f["filename"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(filenames[0], "10_tenth.txt");
    assert_eq!(filenames[1], "1_first.txt");
    assert_eq!(filenames[2], "2_second.txt");
    assert_eq!(filenames[3], "apple.txt");
    assert_eq!(filenames[4], "banana.txt");
    assert_eq!(filenames[5], "zebra.txt");
}

