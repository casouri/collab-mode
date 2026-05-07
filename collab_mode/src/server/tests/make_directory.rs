use super::*;

#[tokio::test]
async fn test_make_directory_local() {
    // Local request: creates dir under a declared project.
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 0, None)
        .await
        .unwrap();

    let project_dir = tempfile::TempDir::new().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    let resp = setup
        .hub
        .editor
        .request(
            "MakeDirectory",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "newdir"
                }
            }),
        )
        .await
        .unwrap();

    assert_eq!(resp["file"]["project"], "TestProject");
    assert_eq!(resp["file"]["file"], "newdir");
    assert!(project_dir.path().join("newdir").is_dir());

    setup.cleanup();
}

#[tokio::test]
async fn test_make_directory_idempotent_and_nested() {
    // Calling twice on the same path is a no-op (create_dir_all),
    // and missing parents are created.
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 0, None)
        .await
        .unwrap();

    let project_dir = tempfile::TempDir::new().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    // Nested path with missing parent.
    setup
        .hub
        .editor
        .request(
            "MakeDirectory",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "a/b/c"
                }
            }),
        )
        .await
        .unwrap();
    assert!(project_dir.path().join("a/b/c").is_dir());

    // Same path again — should still succeed.
    setup
        .hub
        .editor
        .request(
            "MakeDirectory",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "a/b/c"
                }
            }),
        )
        .await
        .unwrap();

    setup.cleanup();
}

#[tokio::test]
async fn test_make_directory_unknown_project() {
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 0, None)
        .await
        .unwrap();

    let result = setup
        .hub
        .editor
        .request(
            "MakeDirectory",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "NoSuchProject",
                    "file": "newdir"
                }
            }),
        )
        .await;

    assert!(result.is_err());

    setup.cleanup();
}

#[tokio::test]
async fn test_make_directory_remote() {
    // Remote request via hub-and-spoke: spoke asks hub to create a dir.
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();

    let project_dir = tempfile::TempDir::new().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .request(
            "MakeDirectory",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "remote-dir"
                }
            }),
        )
        .await
        .unwrap();

    assert_eq!(resp["file"]["hostId"], setup.hub.id.clone());
    assert_eq!(resp["file"]["project"], "TestProject");
    assert_eq!(resp["file"]["file"], "remote-dir");
    assert!(project_dir.path().join("remote-dir").is_dir());

    setup.cleanup();
}

#[tokio::test]
async fn test_make_directory_remote_permission_denied() {
    // Spoke is configured with write=false, so the hub should reject the request.
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(
        &factory,
        1,
        Some(vec![Permission {
            write: false,
            create: false,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    let project_dir = tempfile::TempDir::new().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    let result = setup.spokes[0]
        .editor
        .request(
            "MakeDirectory",
            serde_json::json!({
                "file": {
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "blocked-dir"
                }
            }),
        )
        .await;

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.to_lowercase().contains("permission") || err_msg.to_lowercase().contains("denied"),
        "Expected permission-denied error, got: {}",
        err_msg
    );
    assert!(!project_dir.path().join("blocked-dir").exists());

    setup.cleanup();
}
