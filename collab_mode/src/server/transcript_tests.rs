use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,server=debug,collab_mode=debug")),
        )
        .with_test_writer()
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true)
        .try_init();
}

// Mock document that tracks content and cursor
#[derive(Debug, Clone)]
pub struct MockDocument {
    content: Vec<char>,
    cursor: usize,
}

impl MockDocument {
    pub fn new(initial_content: &str) -> Self {
        MockDocument {
            content: initial_content.chars().collect(),
            cursor: 0,
        }
    }

    pub fn move_cursor(&mut self, offset: i64) {
        let new_pos = if offset >= 0 {
            self.cursor.saturating_add(offset as usize)
        } else {
            self.cursor.saturating_sub(offset.abs() as usize)
        };
        self.cursor = new_pos.min(self.content.len());
    }

    pub fn insert(&mut self, text: &str) -> serde_json::Value {
        let chars: Vec<char> = text.chars().collect();
        for (i, ch) in chars.iter().enumerate() {
            self.content.insert(self.cursor + i, *ch);
        }
        let op = serde_json::json!({
            "op": {
                "Ins": [self.cursor, text]
            },
            "groupSeq": 1
        });
        self.cursor += chars.len();
        op
    }

    pub fn delete(&mut self, count: i64) -> Option<serde_json::Value> {
        if count == 0 {
            return None;
        }

        let (start, end) = if count > 0 {
            // Delete backward
            let start = self.cursor.saturating_sub(count as usize);
            let end = self.cursor;
            (start, end)
        } else {
            // Delete forward
            let start = self.cursor;
            let end = (self.cursor + count.abs() as usize).min(self.content.len());
            (start, end)
        };

        if start == end {
            return None;
        }

        let deleted_text: String = self.content[start..end].iter().collect();
        self.content.drain(start..end);
        self.cursor = start;

        Some(serde_json::json!({
            "op": {
                "Del": [start, deleted_text]
            },
            "groupSeq": 1
        }))
    }

    pub fn apply_remote_op(&mut self, op: &serde_json::Value) -> anyhow::Result<()> {
        if let Some(op_obj) = op.get("op") {
            if let Some(ins) = op_obj.get("Ins") {
                if let Some(arr) = ins.as_array() {
                    if arr.len() == 1 && arr[0].is_array() {
                        // Handle nested array format [[pos, text]]
                        let inner = arr[0].as_array().unwrap();
                        let pos = inner[0].as_u64().unwrap() as usize;
                        let text = inner[1].as_str().unwrap();
                        let chars: Vec<char> = text.chars().collect();
                        for (i, ch) in chars.iter().enumerate() {
                            self.content.insert(pos + i, *ch);
                        }
                    }
                }
            } else if let Some(del) = op_obj.get("Del") {
                if let Some(arr) = del.as_array() {
                    if arr.len() == 1 && arr[0].is_array() {
                        // Handle nested array format [[pos, text]]
                        let inner = arr[0].as_array().unwrap();
                        let pos = inner[0].as_u64().unwrap() as usize;
                        let text = inner[1].as_str().unwrap();
                        let len = text.chars().count();

                        // Verify we're deleting the expected text
                        let actual_text: String = self.content[pos..pos + len].iter().collect();
                        if actual_text != text {
                            return Err(anyhow::anyhow!(
                                "Remote delete mismatch at pos {}: expected '{}', got '{}'",
                                pos,
                                text,
                                actual_text
                            ));
                        }

                        self.content.drain(pos..pos + len);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn get_content(&self) -> String {
        self.content.iter().collect()
    }
}

// Transcript command types
#[derive(Debug, Clone)]
pub enum TranscriptCommand {
    Move { editor: usize, offset: i64 },
    Insert { editor: usize, text: String },
    Delete { editor: usize, count: i64 },
    Undo { editor: usize },
    Redo { editor: usize },
    Send { editor: usize },
    Check,
}

// Parse transcript file
pub fn parse_transcript(
    content: &str,
) -> anyhow::Result<(HashMap<String, String>, Vec<TranscriptCommand>)> {
    let mut lines = content.lines();
    let mut headers = HashMap::new();
    let mut commands = Vec::new();

    // Parse headers until empty line
    let mut in_headers = true;
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            if in_headers {
                in_headers = false;
                break;
            }
            continue;
        }

        if in_headers {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }

    // Parse commands
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line == "===CHECK===" {
            commands.push(TranscriptCommand::Check);
            continue;
        }

        // Parse E<n> <command> format
        if line.starts_with('E') {
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() < 2 {
                continue;
            }

            let editor_num = parts[0][1..].parse::<usize>()?;
            let cmd = parts[1];

            match cmd {
                "MOVE" => {
                    if parts.len() < 3 {
                        return Err(anyhow::anyhow!("MOVE requires offset"));
                    }
                    let offset = parts[2].parse::<i64>()?;
                    commands.push(TranscriptCommand::Move {
                        editor: editor_num - 1, // Convert to 0-indexed
                        offset,
                    });
                }
                "INSERT" => {
                    if parts.len() < 3 {
                        return Err(anyhow::anyhow!("INSERT requires text"));
                    }
                    let text = parts[2].replace("\\n", "\n").replace("\\t", "\t");
                    commands.push(TranscriptCommand::Insert {
                        editor: editor_num - 1,
                        text,
                    });
                }
                "DELETE" => {
                    if parts.len() < 3 {
                        return Err(anyhow::anyhow!("DELETE requires count"));
                    }
                    let count = parts[2].parse::<i64>()?;
                    commands.push(TranscriptCommand::Delete {
                        editor: editor_num - 1,
                        count,
                    });
                }
                "UNDO" => {
                    commands.push(TranscriptCommand::Undo {
                        editor: editor_num - 1,
                    });
                }
                "REDO" => {
                    commands.push(TranscriptCommand::Redo {
                        editor: editor_num - 1,
                    });
                }
                "SEND" => {
                    commands.push(TranscriptCommand::Send {
                        editor: editor_num - 1,
                    });
                }
                _ => {
                    return Err(anyhow::anyhow!("Unknown command: {}", cmd));
                }
            }
        }
    }

    Ok((headers, commands))
}

// Get number of editors needed from commands
pub fn get_editor_count(commands: &[TranscriptCommand]) -> usize {
    let mut max_editor = 0;
    for cmd in commands {
        let editor = match cmd {
            TranscriptCommand::Move { editor, .. }
            | TranscriptCommand::Insert { editor, .. }
            | TranscriptCommand::Delete { editor, .. }
            | TranscriptCommand::Undo { editor }
            | TranscriptCommand::Redo { editor }
            | TranscriptCommand::Send { editor } => *editor,
            TranscriptCommand::Check => continue,
        };
        max_editor = max_editor.max(editor);
    }
    max_editor + 1 // Convert from 0-indexed to count
}

// Run a transcript test
pub async fn run_transcript_test(transcript_path: &str) -> anyhow::Result<()> {
    init_test_tracing();

    // Read transcript file
    let content = std::fs::read_to_string(transcript_path)?;
    let (headers, commands) = parse_transcript(&content)?;

    let test_name = headers
        .get("Name")
        .cloned()
        .unwrap_or_else(|| "Unnamed Test".to_string());
    tracing::info!("Running transcript test: {}", test_name);

    // Determine number of editors needed
    let num_editors = get_editor_count(&commands);
    if num_editors == 0 {
        return Err(anyhow::anyhow!("No editors found in transcript"));
    }

    // Setup test environment
    let env = TestEnvironment::new().await?;
    let mut setup = setup_hub_and_spoke_servers(&env, num_editors).await?;

    // Hub shares the initial file
    let initial_content = ""; // Start with empty document
    let (hub_doc_id, _hub_site_id) = setup
        .hub
        .editor
        .share_file("test.txt", initial_content, serde_json::json!({}))
        .await?;

    // Create spoke editor states and open the shared file
    // We keep the MockDocument separate and just store doc_id and site_id
    let mut spoke_docs = Vec::new();
    let mut spoke_pending_ops: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut spoke_doc_ids = Vec::new();
    let mut spoke_site_ids = Vec::new();
    let mut group_seq_counters: Vec<u32> = vec![1; num_editors];

    for i in 0..num_editors {
        let (doc_id, site_id, content) = setup.spokes[i]
            .editor
            .open_file(
                &setup.hub.id,
                serde_json::json!({
                    "type": "file",
                    "id": hub_doc_id
                }),
            )
            .await?;

        spoke_docs.push(MockDocument::new(&content));
        spoke_pending_ops.push(Vec::new());
        spoke_doc_ids.push(doc_id);
        spoke_site_ids.push(site_id);
    }

    sleep(Duration::from_millis(200)).await;

    // Execute commands
    for (cmd_idx, cmd) in commands.iter().enumerate() {
        tracing::debug!("Executing command {}: {:?}", cmd_idx + 1, cmd);

        match cmd {
            TranscriptCommand::Move { editor, offset } => {
                spoke_docs[*editor].move_cursor(*offset);
            }
            TranscriptCommand::Insert { editor, text } => {
                let group_seq = group_seq_counters[*editor];
                let mut op = spoke_docs[*editor].insert(text);
                // Update the groupSeq in the op
                if let Some(gs) = op.get_mut("groupSeq") {
                    *gs = serde_json::json!(group_seq);
                }
                spoke_pending_ops[*editor].push(op);
                group_seq_counters[*editor] += 1;
            }
            TranscriptCommand::Delete { editor, count } => {
                let group_seq = group_seq_counters[*editor];
                if let Some(mut op) = spoke_docs[*editor].delete(*count) {
                    // Update the groupSeq in the op
                    if let Some(gs) = op.get_mut("groupSeq") {
                        *gs = serde_json::json!(group_seq);
                    }
                    spoke_pending_ops[*editor].push(op);
                    group_seq_counters[*editor] += 1;
                }
            }
            TranscriptCommand::Undo { editor } => {
                // First send pending ops and apply received ops
                if !spoke_pending_ops[*editor].is_empty() {
                    let resp = setup.spokes[*editor]
                        .editor
                        .send_ops(
                            spoke_doc_ids[*editor],
                            &setup.hub.id,
                            spoke_pending_ops[*editor].clone(),
                        )
                        .await?;

                    // Apply remote ops
                    if let Some(ops) = resp.get("ops").and_then(|v| v.as_array()) {
                        for op in ops {
                            spoke_docs[*editor].apply_remote_op(op)?;
                        }
                    }

                    spoke_pending_ops[*editor].clear();
                }

                // Then send undo request
                let ops = setup.spokes[*editor]
                    .editor
                    .send_undo(spoke_doc_ids[*editor], &setup.hub.id, "Undo")
                    .await?;

                // Apply the undo ops locally
                for op in ops {
                    // op is like {"Del": [[pos1, text1], [pos2, text2]]} or {"Ins": [[pos1, text1], ...]}
                    if let Some(del_ops) = op.get("Del").and_then(|v| v.as_array()) {
                        // The Del value is already an array of [pos, text] pairs
                        // We need to wrap it in {"op": {"Del": [[pos, text]]}} format
                        spoke_docs[*editor].apply_remote_op(&serde_json::json!({
                            "op": {"Del": del_ops}
                        }))?;
                    } else if let Some(ins_ops) = op.get("Ins").and_then(|v| v.as_array()) {
                        // The Ins value is already an array of [pos, text] pairs
                        // We need to wrap it in {"op": {"Ins": [[pos, text]]}} format
                        spoke_docs[*editor].apply_remote_op(&serde_json::json!({
                            "op": {"Ins": ins_ops}
                        }))?;
                    }
                }

                // Then send the Undo operation to server
                let group_seq = group_seq_counters[*editor];
                let resp = setup.spokes[*editor]
                    .editor
                    .send_ops(
                        spoke_doc_ids[*editor],
                        &setup.hub.id,
                        vec![serde_json::json!({
                            "op": "Undo",
                            "groupSeq": group_seq
                        })],
                    )
                    .await?;

                // Apply any remote ops from sending Undo
                if let Some(ops) = resp.get("ops").and_then(|v| v.as_array()) {
                    for op in ops {
                        spoke_docs[*editor].apply_remote_op(op)?;
                    }
                }

                group_seq_counters[*editor] += 1;
            }
            TranscriptCommand::Redo { editor } => {
                // First send pending ops and apply received ops
                if !spoke_pending_ops[*editor].is_empty() {
                    let resp = setup.spokes[*editor]
                        .editor
                        .send_ops(
                            spoke_doc_ids[*editor],
                            &setup.hub.id,
                            spoke_pending_ops[*editor].clone(),
                        )
                        .await?;

                    // Apply remote ops
                    if let Some(ops) = resp.get("ops").and_then(|v| v.as_array()) {
                        for op in ops {
                            spoke_docs[*editor].apply_remote_op(op)?;
                        }
                    }

                    spoke_pending_ops[*editor].clear();
                }

                // Then send redo request
                let ops = setup.spokes[*editor]
                    .editor
                    .send_undo(spoke_doc_ids[*editor], &setup.hub.id, "Redo")
                    .await?;

                // Apply the redo ops locally
                for op in ops {
                    // op can be either {"Del": [[pos1, text1], ...]} or {"Ins": [[pos1, text1], ...]}
                    if let Some(del_ops) = op.get("Del").and_then(|v| v.as_array()) {
                        // The Del value is already an array of [pos, text] pairs
                        // We need to wrap it in {"op": {"Del": [[pos, text]]}} format
                        spoke_docs[*editor].apply_remote_op(&serde_json::json!({
                            "op": {"Del": del_ops}
                        }))?;
                    } else if let Some(ins_ops) = op.get("Ins").and_then(|v| v.as_array()) {
                        // The Ins value is already an array of [pos, text] pairs
                        // We need to wrap it in {"op": {"Ins": [[pos, text]]}} format
                        spoke_docs[*editor].apply_remote_op(&serde_json::json!({
                            "op": {"Ins": ins_ops}
                        }))?;
                    }
                }

                // Then send the Redo operation to server
                let group_seq = group_seq_counters[*editor];
                let resp = setup.spokes[*editor]
                    .editor
                    .send_ops(
                        spoke_doc_ids[*editor],
                        &setup.hub.id,
                        vec![serde_json::json!({
                            "op": "Redo",
                            "groupSeq": group_seq
                        })],
                    )
                    .await?;

                // Apply any remote ops from sending Redo
                if let Some(ops) = resp.get("ops").and_then(|v| v.as_array()) {
                    for op in ops {
                        spoke_docs[*editor].apply_remote_op(op)?;
                    }
                }

                group_seq_counters[*editor] += 1;
            }
            TranscriptCommand::Send { editor } => {
                let resp = setup.spokes[*editor]
                    .editor
                    .send_ops(
                        spoke_doc_ids[*editor],
                        &setup.hub.id,
                        spoke_pending_ops[*editor].clone(),
                    )
                    .await?;

                // Apply remote ops
                if let Some(ops) = resp.get("ops").and_then(|v| v.as_array()) {
                    for op in ops {
                        spoke_docs[*editor].apply_remote_op(op)?;
                    }
                }

                spoke_pending_ops[*editor].clear();
                sleep(Duration::from_millis(100)).await;
            }
            TranscriptCommand::Check => {
                // Send all pending ops for all editors
                for i in 0..num_editors {
                    let resp = if !spoke_pending_ops[i].is_empty() {
                        setup.spokes[i]
                            .editor
                            .send_ops(
                                spoke_doc_ids[i],
                                &setup.hub.id,
                                spoke_pending_ops[i].clone(),
                            )
                            .await?
                    } else {
                        // Send empty request to fetch remote ops
                        setup.spokes[i]
                            .editor
                            .send_ops(spoke_doc_ids[i], &setup.hub.id, vec![])
                            .await?
                    };

                    // Apply remote ops
                    if let Some(ops) = resp.get("ops").and_then(|v| v.as_array()) {
                        for op in ops {
                            spoke_docs[i].apply_remote_op(op)?;
                        }
                    }

                    spoke_pending_ops[i].clear();
                }

                sleep(Duration::from_millis(500)).await;

                // Verify all documents have the same content
                if !spoke_docs.is_empty() {
                    let expected_content = spoke_docs[0].get_content();
                    for (i, doc) in spoke_docs.iter().enumerate() {
                        let content = doc.get_content();
                        if content != expected_content {
                            return Err(anyhow::anyhow!(
                                "Content mismatch at CHECK: Editor {} has '{}', expected '{}'",
                                i + 1,
                                content,
                                expected_content
                            ));
                        }
                    }

                    // Also verify hub content (if we can fetch it)
                    // For now we trust that if all spokes agree, the hub should too
                    tracing::info!(
                        "CHECK passed: All {} editors have content: '{}'",
                        spoke_docs.len(),
                        expected_content
                    );
                }
            }
        }
    }

    tracing::info!("Transcript test '{}' completed successfully", test_name);
    setup.cleanup();
    Ok(())
}

// Test discovery - scan transcripts directory and run all tests
#[tokio::test]
async fn test_all_transcripts() {
    let transcripts_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/transcripts");

    if !transcripts_dir.exists() {
        tracing::warn!(
            "Transcripts directory does not exist: {:?}",
            transcripts_dir
        );
        return;
    }

    let mut test_count = 0;
    let mut failed_tests = Vec::new();

    // Read all transcript files
    if let Ok(entries) = std::fs::read_dir(&transcripts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                test_count += 1;
                let test_name = path.file_name().unwrap().to_string_lossy().to_string();

                tracing::info!("Running transcript: {}", test_name);
                match run_transcript_test(path.to_str().unwrap()).await {
                    Ok(_) => {
                        tracing::info!("[PASS] {}", test_name);
                    }
                    Err(e) => {
                        tracing::error!("[FAIL] {}: {}", test_name, e);
                        failed_tests.push((test_name, e));
                    }
                }
            }
        }
    }

    if test_count == 0 {
        tracing::warn!("No transcript tests found in {:?}", transcripts_dir);
        return;
    }

    // Report results
    if !failed_tests.is_empty() {
        let msg = failed_tests
            .iter()
            .map(|(name, err)| format!("  - {}: {}", name, err))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{} transcript test(s) failed:\n{}", failed_tests.len(), msg);
    }

    tracing::info!("All {} transcript tests passed", test_count);
}

// Mock structures from tests.rs - these need to be available
use super::tests::{setup_hub_and_spoke_servers, MockEditor, TestEnvironment};
