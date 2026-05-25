use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

use crate::op::GlobalSeq;
use crate::types::{EditInstruction, EditorFatOp, EditorLeanOp, EditorOp};

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
    ops_applied: usize, // Track number of operations applied
}

impl MockDocument {
    pub fn new(initial_content: &str) -> Self {
        MockDocument {
            content: initial_content.chars().collect(),
            cursor: 0,
            ops_applied: 0,
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

    pub fn insert(&mut self, text: &str) -> EditorFatOp {
        let chars: Vec<char> = text.chars().collect();
        for (i, ch) in chars.iter().enumerate() {
            self.content.insert(self.cursor + i, *ch);
        }
        let pos = self.cursor as u64;
        self.cursor += chars.len();

        EditorFatOp {
            op: EditorOp::Ins {
                pos,
                content: text.to_string(),
            },
            group_seq: 1,
        }
    }

    pub fn delete(&mut self, count: i64) -> Option<EditorFatOp> {
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

        Some(EditorFatOp {
            op: EditorOp::Del {
                pos: start as u64,
                content: deleted_text,
            },
            group_seq: 1,
        })
    }

    /// Apply an EditInstruction to this mock document.
    pub fn apply_edit_instruction(&mut self, instr: &EditInstruction) -> anyhow::Result<()> {
        self.ops_applied += 1;

        match instr {
            EditInstruction::Ins { edits } => {
                // Collect and convert u64 positions to usize
                let mut inserts: Vec<(usize, String)> = edits
                    .iter()
                    .map(|edit| (edit.pos as usize, edit.content.clone()))
                    .collect();

                // Apply inserts in reverse order to avoid position shifting
                inserts.reverse();
                let mut last_insert_end_pos: i64 = -1;
                let mut first_iter = true;

                for (pos, text) in inserts {
                    let doc_len = self.content.len();
                    tracing::debug!(
                        pos,
                        doc_len,
                        text_len = text.chars().count(),
                        ops_applied = self.ops_applied,
                        "MockDocument applying Ins"
                    );
                    if pos > doc_len {
                        return Err(anyhow::anyhow!(
                            "Insert position {} out of bounds (doc_len: {}, text: '{}')",
                            pos,
                            doc_len,
                            text
                        ));
                    }
                    let chars: Vec<char> = text.chars().collect();
                    for (i, ch) in chars.iter().enumerate() {
                        self.content.insert(pos + i, *ch);
                    }
                    if first_iter {
                        last_insert_end_pos = (pos as i64) + (text.chars().count() as i64);
                        first_iter = false;
                    } else {
                        last_insert_end_pos += text.chars().count() as i64;
                    }
                }

                // Position cursor at the end of the insert
                if last_insert_end_pos > -1 {
                    self.cursor = last_insert_end_pos as usize;
                }
            }
            EditInstruction::Del { edits } => {
                // Collect and convert u64 positions to usize
                let mut deletes: Vec<(usize, String)> = edits
                    .iter()
                    .map(|edit| (edit.pos as usize, edit.content.clone()))
                    .collect();

                // Track the minimum position for cursor positioning
                let min_pos = deletes.iter().map(|(pos, _)| *pos).min();

                // Apply deletes in reverse order to avoid position shifting
                deletes.reverse();

                for (pos, text) in deletes {
                    let len = text.chars().count();

                    // Verify we're deleting the expected text
                    let actual_text: String = self.content[pos..pos + len].iter().collect();
                    if actual_text != text {
                        return Err(anyhow::anyhow!(
                            "Delete mismatch at pos {}: expected '{}', got '{}'",
                            pos,
                            text,
                            actual_text
                        ));
                    }

                    self.content.drain(pos..pos + len);
                }

                // Position cursor at the beginning of the first delete
                if let Some(pos) = min_pos {
                    self.cursor = pos;
                }
            }
        }

        Ok(())
    }

    pub fn apply_remote_op(&mut self, op: &serde_json::Value) -> anyhow::Result<()> {
        // Parse JSON into EditorLeanOp
        let lean_op: EditorLeanOp = serde_json::from_value(op.clone())
            .map_err(|e| anyhow::anyhow!("Failed to parse EditorLeanOp: {}", e))?;

        // Apply the instruction using the helper
        self.apply_edit_instruction(&lean_op.op)
    }

    pub fn get_content(&self) -> String {
        self.content.iter().collect()
    }

    pub fn get_ops_applied(&self) -> usize {
        self.ops_applied
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
    BeginGroup { editor: usize },
    EndGroup { editor: usize },
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
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }

        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_string(), value.trim().to_string());
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
                "BEGIN_GROUP" => {
                    commands.push(TranscriptCommand::BeginGroup {
                        editor: editor_num - 1,
                    });
                }
                "END_GROUP" => {
                    commands.push(TranscriptCommand::EndGroup {
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
            | TranscriptCommand::Send { editor }
            | TranscriptCommand::BeginGroup { editor }
            | TranscriptCommand::EndGroup { editor } => *editor,
            TranscriptCommand::Check => continue,
        };
        max_editor = max_editor.max(editor);
    }
    max_editor + 1 // Convert from 0-indexed to count
}

// Helper function to send pending ops and apply remote ops
async fn send_ops_and_apply_remote(
    setup: &mut HubAndSpokeSetup,
    editor_idx: usize,
    spoke_files: &[serde_json::Value],
    spoke_pending_ops: &mut [Vec<serde_json::Value>],
    spoke_docs: &mut [MockDocument],
    spoke_ops_counts: &mut [usize],
) -> anyhow::Result<()> {
    // Always send ops (even if empty) to fetch remote ops
    let send_resp = setup.spokes[editor_idx]
        .editor
        .send_ops(
            spoke_files[editor_idx].clone(),
            spoke_pending_ops[editor_idx].clone(),
        )
        .await?;

    // Apply remote ops
    for lean_op in &send_resp.ops {
        spoke_docs[editor_idx].apply_edit_instruction(&lean_op.op)?;
    }
    spoke_ops_counts[editor_idx] = spoke_docs[editor_idx].get_ops_applied();
    spoke_pending_ops[editor_idx].clear();

    // Check if doc length matches server's gapbuf length
    let mock_len = spoke_docs[editor_idx].get_content().len() as u64;
    if mock_len != send_resp.doc_len {
        return Err(anyhow::anyhow!(
            "MISMATCH: Editor {} MockDocument length {} != server gapbuf length {}",
            editor_idx + 1,
            mock_len,
            send_resp.doc_len
        ));
    }

    Ok(())
}

// Helper function to validate content sync between MockDocument and server
async fn validate_content_sync(
    setup: &mut HubAndSpokeSetup,
    editor_idx: usize,
    spoke_files: &[serde_json::Value],
    spoke_docs: &[MockDocument],
) -> anyhow::Result<()> {
    // Send OpenFile request to get current server content
    let (_file_desc, _site_id, server_content) = setup.spokes[editor_idx]
        .editor
        .open_file(spoke_files[editor_idx].clone())
        .await?;

    let mock_content = spoke_docs[editor_idx].get_content();

    if server_content != mock_content {
        return Err(anyhow::anyhow!(
            "Content sync error for Editor {}: server has '{}' ({} chars), mock has '{}' ({} chars)",
            editor_idx + 1,
            server_content,
            server_content.len(),
            mock_content,
            mock_content.len()
        ));
    }

    tracing::debug!(
        "Content validation passed for Editor {}: {} chars",
        editor_idx + 1,
        server_content.len()
    );

    Ok(())
}

// Macro to handle errors with diagnostic dumping
macro_rules! handle_error {
    ($result:expr, $diagnostics:expr, $spoke_docs:expr, $spoke_pending_ops:expr, $spoke_ops_counts:expr, $group_seq_counters:expr) => {
        match $result {
            Ok(val) => val,
            Err(e) => {
                $diagnostics.dump_diagnostics(
                    &e,
                    $spoke_docs,
                    $spoke_pending_ops,
                    $spoke_ops_counts,
                    $group_seq_counters,
                );
                return Err(e);
            }
        }
    };
}

// Diagnostic state for error reporting
struct DiagnosticState {
    command_history: Vec<(usize, TranscriptCommand)>,
    max_history: usize,
}

impl DiagnosticState {
    fn new(max_history: usize) -> Self {
        DiagnosticState {
            command_history: Vec::new(),
            max_history,
        }
    }

    fn record_command(&mut self, idx: usize, cmd: TranscriptCommand) {
        self.command_history.push((idx, cmd));
        if self.command_history.len() > self.max_history {
            self.command_history.remove(0);
        }
    }

    fn dump_diagnostics(
        &self,
        error: &anyhow::Error,
        spoke_docs: &[MockDocument],
        spoke_pending_ops: &[Vec<serde_json::Value>],
        spoke_ops_counts: &[usize],
        group_seq_counters: &[u32],
    ) {
        eprintln!("\n{}", "=".repeat(80));
        eprintln!("ERROR DIAGNOSTICS");
        eprintln!("{}", "=".repeat(80));
        eprintln!("Error: {}", error);
        eprintln!();

        // Dump command history
        eprintln!(
            "Command History (last {} commands):",
            self.command_history.len()
        );
        for (idx, cmd) in &self.command_history {
            eprintln!("  [{}] {:?}", idx + 1, cmd);
        }
        eprintln!();

        // Dump mock document states
        eprintln!("Mock Document States:");
        for (i, doc) in spoke_docs.iter().enumerate() {
            let content = doc.get_content();
            eprintln!("  Editor {}:", i + 1);
            eprintln!("    Content: '{}'", content);
            eprintln!("    Length: {} chars", content.len());
            eprintln!("    Cursor: {}", doc.cursor);
            eprintln!("    Ops applied: {}", spoke_ops_counts[i]);
            eprintln!("    Pending ops: {}", spoke_pending_ops[i].len());
            eprintln!("    Group seq: {}", group_seq_counters[i]);
        }
        eprintln!();

        // Dump pending operations
        eprintln!("Pending Operations:");
        for (i, pending) in spoke_pending_ops.iter().enumerate() {
            if !pending.is_empty() {
                eprintln!("  Editor {}:", i + 1);
                for (j, op) in pending.iter().enumerate() {
                    eprintln!("    [{}] {}", j, op);
                }
            }
        }
        eprintln!("{}\n", "=".repeat(80));
    }
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
    let factory = TestChannelFactory::new();
    let mut setup = match setup_hub_and_spoke_servers(&factory, num_editors, None).await {
        Ok(s) => s,
        Err(e) => return Err(e),
    };

    // Run the test, ensuring cleanup happens regardless of success or failure
    let result = async {
        // Hub shares the initial file
        let initial_content = ""; // Start with empty document
        let (hub_file, _hub_site_id) = setup
            .hub
            .editor
            .share_file("test.txt", initial_content, serde_json::json!({}))
            .await?;

        // Create spoke editor states and open the shared file
        // We keep the MockDocument separate and just store doc_id and site_id
        let mut spoke_docs = Vec::new();
        let mut spoke_pending_ops: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut spoke_files = Vec::new();
        let mut spoke_site_ids = Vec::new();
        let mut group_seq_counters: Vec<u32> = vec![1; num_editors];
        let mut in_group: Vec<bool> = vec![false; num_editors];  // Track if editor is in a group
        let mut spoke_ops_counts: Vec<usize> = vec![0; num_editors];  // Track ops applied by each editor
        let mut spoke_global_seqs: Vec<GlobalSeq> = vec![0; num_editors];  // Track last_global_seq from server
        let mut spoke_pending_counts: Vec<usize> = vec![0; num_editors];  // Track pending_local_ops from server

        for i in 0..num_editors {
            let (file_desc, site_id, content) =
                setup.spokes[i].editor.open_file(hub_file.clone()).await?;

            spoke_docs.push(MockDocument::new(&content));
            spoke_pending_ops.push(Vec::new());
            spoke_files.push(file_desc);
            spoke_site_ids.push(site_id);
        }

        sleep(Duration::from_millis(200)).await;

        // Initialize diagnostic state for error tracking
        let mut diagnostics = DiagnosticState::new(10);

        // Execute commands
        for (cmd_idx, cmd) in commands.iter().enumerate() {
            tracing::debug!("Executing command {}: {:?}", cmd_idx + 1, cmd);

            // Record command in history before executing
            diagnostics.record_command(cmd_idx, cmd.clone());

            match cmd {
                TranscriptCommand::Move { editor, offset } => {
                    spoke_docs[*editor].move_cursor(*offset);
                }
                TranscriptCommand::Insert { editor, text } => {
                    let group_seq = group_seq_counters[*editor];
                    let mut op = spoke_docs[*editor].insert(text);
                    // Update the groupSeq in the op
                    op.group_seq = group_seq;
                    spoke_pending_ops[*editor].push(serde_json::to_value(&op).unwrap());
                    if !in_group[*editor] {
                        group_seq_counters[*editor] += 1;
                    }
                }
                TranscriptCommand::Delete { editor, count } => {
                    let group_seq = group_seq_counters[*editor];
                    if let Some(mut op) = spoke_docs[*editor].delete(*count) {
                        // Update the groupSeq in the op
                        op.group_seq = group_seq;
                        spoke_pending_ops[*editor].push(serde_json::to_value(&op).unwrap());
                        if !in_group[*editor] {
                            group_seq_counters[*editor] += 1;
                        }
                    }
                }
                TranscriptCommand::Undo { editor } => {
                    // First send pending ops and apply received ops
                    handle_error!(
                        send_ops_and_apply_remote(
                            &mut setup,
                            *editor,
                            &spoke_files,
                            &mut spoke_pending_ops,
                            &mut spoke_docs,
                            &mut spoke_ops_counts,
                        )
                        .await,
                        &diagnostics,
                        &spoke_docs,
                        &spoke_pending_ops,
                        &spoke_ops_counts,
                        &group_seq_counters
                    );

                    // Then send undo request
                    let undo_resp = handle_error!(
                        setup.spokes[*editor]
                            .editor
                            .send_undo(spoke_files[*editor].clone(), "Undo")
                            .await,
                        &diagnostics,
                        &spoke_docs,
                        &spoke_pending_ops,
                        &spoke_ops_counts,
                        &group_seq_counters
                    );

                    // Only send the Undo operation to server if there were actual ops to undo
                    if !undo_resp.ops.is_empty() {
                        // Apply the undo ops locally (server doesn't return them in send_ops response)
                        for instr in &undo_resp.ops {
                            handle_error!(
                                spoke_docs[*editor].apply_edit_instruction(instr),
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            );
                        }
                        spoke_ops_counts[*editor] = spoke_docs[*editor].get_ops_applied();

                        let group_seq = group_seq_counters[*editor];
                        let send_resp = handle_error!(
                            setup.spokes[*editor]
                                .editor
                                .send_ops(
                                    spoke_files[*editor].clone(),
                                    vec![serde_json::json!({
                                        "op": { "kind": "Undo", "context": undo_resp.context },
                                        "groupSeq": group_seq
                                    })],
                                )
                                .await,
                            &diagnostics,
                            &spoke_docs,
                            &spoke_pending_ops,
                            &spoke_ops_counts,
                            &group_seq_counters
                        );

                        // Apply any remote ops from other editors
                        for lean_op in &send_resp.ops {
                            handle_error!(
                                spoke_docs[*editor].apply_edit_instruction(&lean_op.op),
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            );
                        }
                        spoke_ops_counts[*editor] = spoke_docs[*editor].get_ops_applied();

                        group_seq_counters[*editor] += 1;
                    }
                }
                TranscriptCommand::Redo { editor } => {
                    // First send pending ops and apply received ops
                    handle_error!(
                        send_ops_and_apply_remote(
                            &mut setup,
                            *editor,
                            &spoke_files,
                            &mut spoke_pending_ops,
                            &mut spoke_docs,
                            &mut spoke_ops_counts,
                        )
                        .await,
                        &diagnostics,
                        &spoke_docs,
                        &spoke_pending_ops,
                        &spoke_ops_counts,
                        &group_seq_counters
                    );

                    // Then send redo request
                    let redo_resp = handle_error!(
                        setup.spokes[*editor]
                            .editor
                            .send_undo(spoke_files[*editor].clone(), "Redo")
                            .await,
                        &diagnostics,
                        &spoke_docs,
                        &spoke_pending_ops,
                        &spoke_ops_counts,
                        &group_seq_counters
                    );

                    // Only send the Redo operation to server if there were actual ops to redo
                    if !redo_resp.ops.is_empty() {
                        // Apply the redo ops locally (server doesn't return them in send_ops response)
                        for instr in &redo_resp.ops {
                            handle_error!(
                                spoke_docs[*editor].apply_edit_instruction(instr),
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            );
                        }
                        spoke_ops_counts[*editor] = spoke_docs[*editor].get_ops_applied();

                        let group_seq = group_seq_counters[*editor];
                        let send_resp = handle_error!(
                            setup.spokes[*editor]
                                .editor
                                .send_ops(
                                    spoke_files[*editor].clone(),
                                    vec![serde_json::json!({
                                        "op": { "kind": "Redo", "context": redo_resp.context },
                                        "groupSeq": group_seq
                                    })],
                                )
                                .await,
                            &diagnostics,
                            &spoke_docs,
                            &spoke_pending_ops,
                            &spoke_ops_counts,
                            &group_seq_counters
                        );

                        // Apply any remote ops from other editors
                        for lean_op in &send_resp.ops {
                            handle_error!(
                                spoke_docs[*editor].apply_edit_instruction(&lean_op.op),
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            );
                        }
                        spoke_ops_counts[*editor] = spoke_docs[*editor].get_ops_applied();

                        group_seq_counters[*editor] += 1;
                    }
                }
                TranscriptCommand::Send { editor } => {
                    handle_error!(
                        send_ops_and_apply_remote(
                            &mut setup,
                            *editor,
                            &spoke_files,
                            &mut spoke_pending_ops,
                            &mut spoke_docs,
                            &mut spoke_ops_counts,
                        )
                        .await,
                        &diagnostics,
                        &spoke_docs,
                        &spoke_pending_ops,
                        &spoke_ops_counts,
                        &group_seq_counters
                    );

                    // Validate content sync after sending ops
                    handle_error!(
                        validate_content_sync(
                            &mut setup,
                            *editor,
                            &spoke_files,
                            &spoke_docs,
                        )
                        .await,
                        &diagnostics,
                        &spoke_docs,
                        &spoke_pending_ops,
                        &spoke_ops_counts,
                        &group_seq_counters
                    );

                }
                TranscriptCommand::BeginGroup { editor } => {
                    in_group[*editor] = true;
                }
                TranscriptCommand::EndGroup { editor } => {
                    in_group[*editor] = false;
                    group_seq_counters[*editor] += 1;
                }
                TranscriptCommand::Check => {
                    // First round: Send all pending ops for all editors
                    for i in 0..num_editors {
                        let send_resp = if !spoke_pending_ops[i].is_empty() {
                            handle_error!(
                                setup.spokes[i]
                                    .editor
                                    .send_ops(spoke_files[i].clone(), spoke_pending_ops[i].clone())
                                    .await,
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            )
                        } else {
                            // Send empty request to fetch remote ops
                            handle_error!(
                                setup.spokes[i]
                                    .editor
                                    .send_ops(spoke_files[i].clone(), vec![])
                                    .await,
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            )
                        };

                        // Apply remote ops
                        for lean_op in &send_resp.ops {
                            handle_error!(
                                spoke_docs[i].apply_edit_instruction(&lean_op.op),
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            );
                        }
                        // Update ops count
                        spoke_ops_counts[i] = spoke_docs[i].get_ops_applied();

                        spoke_pending_ops[i].clear();
                    }

                    // Convergence loop: poll until all editors have same last_global_seq and zero pending_local_ops
                    for attempt in 0..20 {
                        sleep(Duration::from_millis(100)).await;

                        // Send SendOps to hub first to force it to process any pending messages
                        // The hub might have operations from spokes in its message queue that haven't been processed yet
                        let _ = setup.hub.editor.send_ops(hub_file.clone(), vec![]).await;

                        // Pull more ops for each editor and update tracking
                        for i in 0..num_editors {
                            let send_resp = handle_error!(
                                setup.spokes[i]
                                    .editor
                                    .send_ops(spoke_files[i].clone(), vec![])
                                    .await,
                                &diagnostics,
                                &spoke_docs,
                                &spoke_pending_ops,
                                &spoke_ops_counts,
                                &group_seq_counters
                            );

                            // Apply any remote ops
                            for lean_op in &send_resp.ops {
                                handle_error!(
                                    spoke_docs[i].apply_edit_instruction(&lean_op.op),
                                    &diagnostics,
                                    &spoke_docs,
                                    &spoke_pending_ops,
                                    &spoke_ops_counts,
                                    &group_seq_counters
                                );
                            }

                            // Update tracking info
                            spoke_ops_counts[i] = spoke_docs[i].get_ops_applied();
                            spoke_global_seqs[i] = send_resp.last_global_seq;
                            spoke_pending_counts[i] = send_resp.pending_local_ops;
                        }

                        // Check convergence: all editors have same last_global_seq AND all have pending_local_ops == 0
                        let first_seq = spoke_global_seqs[0];
                        let all_same_seq = spoke_global_seqs.iter().all(|&seq| seq == first_seq);
                        let all_zero_pending = spoke_pending_counts.iter().all(|&pending| pending == 0);

                        if all_same_seq && all_zero_pending {
                            tracing::debug!(
                                "CHECK: Editors converged after {} attempts. global_seq: {}, ops_counts: {:?}",
                                attempt,
                                first_seq,
                                spoke_ops_counts
                            );
                            break;
                        }

                        if attempt == 19 {
                            return Err(anyhow::anyhow!(
                                "CHECK failed: editors not converged after 20 attempts. global_seqs: {:?}, pending_counts: {:?}, ops_counts: {:?}",
                                spoke_global_seqs,
                                spoke_pending_counts,
                                spoke_ops_counts
                            ));
                        }

                        tracing::debug!(
                            "CHECK attempt {}: global_seqs: {:?}, pending_counts: {:?}",
                            attempt,
                            spoke_global_seqs,
                            spoke_pending_counts
                        );
                    }

                    // Verify all documents have the same content
                    if !spoke_docs.is_empty() {
                        let expected_content = spoke_docs[0].get_content();
                        for (i, doc) in spoke_docs.iter().enumerate() {
                            let content = doc.get_content();
                            if content != expected_content {
                                return Err(anyhow::anyhow!(
                                    "Content mismatch at CHECK: Editor {} has '{}', expected '{}'. ops_counts: {:?}",
                                    i + 1,
                                    content,
                                    expected_content,
                                    spoke_ops_counts
                                ));
                            }
                        }

                        tracing::info!(
                            "CHECK passed: All {} editors have content: '{}' with ops_counts: {:?}",
                            spoke_docs.len(),
                            expected_content,
                            spoke_ops_counts
                        );
                    }
                }
            }
        }

        tracing::info!("Transcript test '{}' completed successfully", test_name);

        // Print editor results if COLLAB_TRANSCRIPT_PRINT_RESULT is set
        if std::env::var("COLLAB_TRANSCRIPT_PRINT_RESULT").is_ok() {
            println!("\n=== Transcript Results for '{}' ===", test_name);
            for (i, doc) in spoke_docs.iter().enumerate() {
                println!("Editor {}: '{}'", i + 1, doc.get_content());
            }
            println!("ops_counts: {:?}", spoke_ops_counts);
            println!("=====================================\n");
        }

        Ok(())
    }
    .await;

    // Always cleanup, regardless of test result
    setup.cleanup();

    result
}

// Test discovery: scan transcripts directory and run all tests.
// Ignored by default since it requires single thread and is slow.
#[tokio::test]
#[ignore]
async fn test_all_transcripts() {
    let transcripts_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/transcripts");

    if !transcripts_dir.exists() {
        tracing::warn!(
            "Transcripts directory does not exist: {:?}",
            transcripts_dir
        );
        return;
    }

    // Check if we should run a specific transcript
    let specific_transcript = std::env::var("COLLAB_TRANSCRIPT").ok();

    let mut test_count = 0;
    let mut failed_tests = Vec::new();

    // Read all transcript files
    if let Ok(entries) = std::fs::read_dir(&transcripts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                let test_name = path.file_name().unwrap().to_string_lossy().to_string();

                // If COLLAB_TRANSCRIPT is set, only run that specific transcript
                if let Some(ref filter) = specific_transcript {
                    // Support with or without .txt extension
                    let filter_with_ext = if filter.ends_with(".txt") {
                        filter.clone()
                    } else {
                        format!("{}.txt", filter)
                    };

                    if test_name != *filter && test_name != filter_with_ext {
                        continue;
                    }
                }

                test_count += 1;

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

                // Small delay between tests to allow resources to be released
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    if test_count == 0 {
        if let Some(filter) = specific_transcript {
            panic!("No transcript found matching: {}", filter);
        } else {
            tracing::warn!("No transcript tests found in {:?}", transcripts_dir);
            return;
        }
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
use super::tests::{setup_hub_and_spoke_servers, HubAndSpokeSetup, TestChannelFactory};
