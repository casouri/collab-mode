use super::*;

#[tokio::test]
async fn test_undo_e2e() {
    // End-to-end test for Undo/Redo functionality.
    let factory = TestChannelFactory::new();
    let mut setup = setup_hub_and_spoke_servers(&factory, 1, None)
        .await
        .unwrap();

    // Create test project with a file.
    let project_dir = super::create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project.
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    // Spoke opens the file.
    let (file_desc, _site_id, content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(content, "Hello from test.txt");

    // Send an insert operation.
    let send_ops_resp = setup.spokes[0]
        .editor
        .send_ops(
            file_desc.clone(),
            vec![serde_json::json!({
                "op": { "kind": "Ins", "pos": 0, "content": "UNDO_TEST: " },
                "groupSeq": 1
            })],
        )
        .await
        .unwrap();
    tracing::info!("SendOps response: {:?}", send_ops_resp);

    // Send an undo request and get the undo operations.
    let undo_ops = setup.spokes[0]
        .editor
        .send_undo(file_desc.clone(), "Undo")
        .await
        .unwrap();

    assert!(!undo_ops.ops.is_empty());
    let actual_op = &undo_ops.ops[0];
    let expected_op = crate::types::EditInstruction::Del {
        edits: vec![crate::types::Edit {
            pos: 0,
            content: "UNDO_TEST: ".to_string(),
        }],
    };
    assert_eq!(actual_op, &expected_op);

    // Apply the undo by sending an Undo op via SendOps.
    let _ = setup.spokes[0]
        .editor
        .send_ops(
            file_desc.clone(),
            vec![serde_json::json!({ "op": { "kind": "Undo", "context": undo_ops.context }, "groupSeq": 2 })],
        )
        .await
        .unwrap();

    // Test redo.
    let redo_ops = setup.spokes[0]
        .editor
        .send_undo(file_desc.clone(), "Redo")
        .await
        .unwrap();
    assert!(!redo_ops.ops.is_empty());
    let actual_redo_op = &redo_ops.ops[0];
    let expected_redo_op = crate::types::EditInstruction::Ins {
        edits: vec![crate::types::Edit {
            pos: 0,
            content: "UNDO_TEST: ".to_string(),
        }],
    };
    assert_eq!(actual_redo_op, &expected_redo_op);

    setup.cleanup();
}
