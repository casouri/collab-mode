use super::*;

#[tokio::test]
async fn test_expand_project_paths_home_directory() {
    // Paths starting with ~ should be expanded to home directory.
    use crate::config_man::ConfigProject;
    use std::env;

    let home_dir = env::var("HOME").unwrap_or_else(|_| "CAN’t GET HOME DIR".to_string());

    let mut projects = vec![
        ConfigProject {
            name: "home_project".to_string(),
            path: "~/my_project".to_string(),
        },
        ConfigProject {
            name: "nested_home".to_string(),
            path: "~/Documents/code/project".to_string(),
        },
    ];

    // Call the function under test (re-exported via tests module).
    let result = super::expand_project_paths(&mut projects);
    assert!(result.is_ok());

    assert_eq!(projects[0].path, format!("{}/my_project", home_dir));
    assert_eq!(
        projects[1].path,
        format!("{}/Documents/code/project", home_dir)
    );

    for project in &projects {
        assert!(std::path::Path::new(&project.path).is_absolute());
    }
}

#[tokio::test]
async fn test_expand_project_paths_relative_error() {
    // Relative paths should cause an error.
    use crate::config_man::ConfigProject;

    let mut projects = vec![ConfigProject {
        name: "relative_project".to_string(),
        path: "./my_project".to_string(),
    }];

    let result = super::expand_project_paths(&mut projects);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("is not absolute"));
    assert!(
        err.to_string()
            .contains("All project paths must be absolute")
    );
}

#[tokio::test]
async fn test_declare_projects_relative_path_error() {
    // DeclareProjects should return error for relative paths.
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Try to declare a project with a relative path.
    let result = setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "name": "relative_project",
                    "path": "./my_relative_project"
                }]
            }),
        )
        .await;

    assert!(result.is_err());
    setup.cleanup();
}

#[tokio::test]
async fn test_server_run_config_projects_expansion() {
    // Projects from config should be expanded when server starts.
    use crate::config_man::{AcceptMode, Config, ConfigManager, ConfigProject};
    use std::collections::HashMap;

    super::init_test_tracing();

    // Create a temp directory for config.
    let temp_dir = tempfile::TempDir::new().unwrap();
    let config_path = temp_dir.path().to_path_buf();

    // Create a test project directory.
    let project_dir = super::create_test_project().unwrap();

    // Create config with projects using ~ path.
    let config = Config {
        projects: vec![
            ConfigProject {
                name: "home_project".to_string(),
                path: "~/my_test_project".to_string(),
            },
            ConfigProject {
                name: "absolute_project".to_string(),
                path: project_dir.path().to_string_lossy().to_string(),
            },
        ],
        trusted_hosts: HashMap::new(),
        accept_mode: AcceptMode::All,
        host_id: Some("test-server".to_string()),
        permission: HashMap::new(),
    };

    // Create ConfigManager.
    let mut config_manager = ConfigManager::new(Some(config_path), None).unwrap();
    config_manager.replace_and_save(config).unwrap();

    // Create and run server.
    let host_id = "test-server".to_string();
    let mut server = Server::new(host_id.clone(), config_manager).unwrap();

    // Create channels for server.
    let (editor_to_server_tx, editor_to_server_rx) = mpsc::channel(100);
    let (server_to_editor_tx, mut server_to_editor_rx) = mpsc::channel(100);

    // Run server in background task.
    let server_task =
        tokio::spawn(async move { server.run(server_to_editor_tx, editor_to_server_rx).await });

    // Send Initialize request to verify server is running.
    let init_request = lsp_server::Request {
        id: lsp_server::RequestId::from(1),
        method: "Initialize".to_string(),
        params: serde_json::json!({}),
    };

    editor_to_server_tx
        .send(lsp_server::Message::Request(init_request))
        .await
        .unwrap();

    // Wait for response.
    let timeout_duration = Duration::from_secs(1);
    let response = tokio::time::timeout(timeout_duration, server_to_editor_rx.recv()).await;
    assert!(response.is_ok(), "Should receive response from server");

    if let Ok(Some(lsp_server::Message::Response(resp))) = response {
        assert!(resp.error.is_none(), "Initialize should succeed");
        let result = resp.result.unwrap();
        assert_eq!(result["hostId"], "test-server");
    } else {
        panic!("Expected Initialize response");
    }

    server_task.abort();
}

