use super::*;

#[tokio::test]
async fn test_share_file_e2e() {
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Share a file with some content.
    let (file_desc, site_id) = setup
        .hub
        .editor
        .share_file(
            "test_file.txt",
            "Hello, this is shared content!",
            serde_json::json!({"author": "test"}),
        )
        .await
        .unwrap();

    assert!(file_desc.is_object());
    assert_eq!(site_id, 0);

    // Send ops to the shared doc.
    let ops = vec![serde_json::json!({
        "op": {"Ins": [30, " More text."]},
        "groupSeq": 1
    })];
    let _ = setup.hub.editor.send_ops(file_desc.clone(), ops).await.unwrap();

    // Share another file and verify doc id differs.
    let (file_desc2, site_id2) = setup
        .hub
        .editor
        .share_file("another_file.txt", "Different content", serde_json::json!({}))
        .await
        .unwrap();
    assert_ne!(file_desc, file_desc2);
    assert_eq!(site_id2, 0);

    setup.cleanup();
}

