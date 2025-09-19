use super::*;

#[tokio::test]
async fn test_list_files_top_level() {
    // Test: List top-level projects and virtual projects.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create two test projects.
    let project1_dir = super::create_test_project().unwrap();
    let project1_path = project1_dir.path().to_string_lossy().to_string();
    let project2_dir = super::create_complex_project().unwrap();
    let project2_path = project2_dir.path().to_string_lossy().to_string();

    // Hub declares projects.
    setup
        .hub
        .editor
        .declare_project(&project1_path, "Project1")
        .await
        .unwrap();
    setup
        .hub
        .editor
        .declare_project(&project2_path, "Project2")
        .await
        .unwrap();

    // Spoke requests top-level listing.
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListProjects",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "signalingAddr": env.signaling_url()
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

    // 2 projects + _buffers + _files.
    assert_eq!(files.len(), 4);

    let has_project1 = files.iter().any(|f| {
        f["filename"].as_str() == Some("Project1")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["file"].as_str() == Some("")
    });
    assert!(has_project1);

    let has_project2 = files.iter().any(|f| {
        f["filename"].as_str() == Some("Project2")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["file"].as_str() == Some("")
    });
    assert!(has_project2);

    let has_buffers = files.iter().any(|f| {
        f["filename"].as_str() == Some("_buffers")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["file"].as_str() == Some("")
    });
    assert!(has_buffers);

    let has_files = files.iter().any(|f| {
        f["filename"].as_str() == Some("_files")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["file"].as_str() == Some("")
    });
    assert!(has_files);

    setup.cleanup();
}
