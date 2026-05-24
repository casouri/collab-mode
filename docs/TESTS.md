# Test summary

A map of every test in this repository, grouped by module, with the command to run each batch. Tests live in two trees:

- **Rust** (`collab_mode/`): unit tests inside each module plus integration-style tests under `src/server/tests/`
- **Emacs Lisp** (`lisp/`, `test/`): ERT unit tests and transcript-driven edit-tracking tests

## Running everything

```sh
# All Rust tests, single-threaded (avoids port collisions / fd exhaustion).
make -C collab_mode test
# Equivalent: cargo test -- --test-threads=1
```

`cargo test -p collab_mode <filter>` runs whatever subset of tests has `<filter>` in its full path (e.g. `cargo test -p collab_mode io_channel` runs everything under `io_channel::*`).

## Rust modules and their tests

### `op` — operational-transform primitives
File: `collab_mode/src/op.rs` (inline `mod tests`)

- `test_transform_ii` — Ins-vs-Ins transform
- `test_transform_di` — Del-vs-Ins transform
- (Several other transform/skip cases are commented out as TODO.)

Run: `cargo test -p collab_mode op::tests`

### `simple_op` — minimal OT reference impl
File: `collab_mode/src/simple_op.rs`

- `test_simple_transform`

Run: `cargo test -p collab_mode simple_op`

### `engine` — client/server OT engines, internal doc model
Files: `collab_mode/src/engine.rs` (inline `mod tests`), `collab_mode/src/engine/tests/internal_doc_tests.rs`

Top-level engine tests (in `engine.rs`):
- `deopt_puzzle` — deOPT puzzle (II.c reference)
- `false_tie_puzzle`
- `undo` — Emacs `{` insert/auto-paren undo case
- `test_convert_pos`, `test_apply_ins`, `test_apply_mark_dead`, `test_apply_mark_live`, `test_mark_to_ins_or_del`

Internal-doc tests (`engine/tests/internal_doc_tests.rs`, ~28 tests):
- Doc construction & range methods: `test_internal_doc_new`, `test_range_methods`, `test_editor_len`, `test_get_cursor`
- Position conversion: `test_convert_pos_empty_doc`, `test_convert_pos_simple`, `test_convert_pos_with_tombstones`
- Cursor handling: `test_shift_cursors_right`, `test_cursor_movement`, `test_cursor_pos_helper`, `test_move_to_beg`, `test_multiple_cursors_concurrent_edits`
- Apply ops: `test_apply_ins_empty_doc`, `test_apply_ins_into_live_range`, `test_apply_ins_at_dead_range_boundaries`, `test_apply_ins_split_dead_range`
- Mark dead/live: `test_apply_mark_dead_simple`, `test_apply_mark_dead_already_dead`, `test_apply_mark_dead_complex`, `test_apply_mark_live_simple`, `test_apply_mark_live_already_live`
- Op conversion: `test_convert_editor_op_ins`, `test_convert_editor_op_del`, `test_convert_internal_op_ins`, `test_convert_internal_op_mark_dead`, `test_convert_internal_op_mark_live`
- Range / boundary: `test_range_iterator`, `test_range_iterator_edge_cases`, `test_boundary_conditions`, `test_coalescing_ranges`
- Misc: `test_unicode_handling`, `test_complex_scenario`

Run: `cargo test -p collab_mode engine`

### `io_channel` — framed message channel over `AsyncRead`/`AsyncWrite`
File: `collab_mode/src/io_channel/tests.rs`

- `roundtrip` — basic send/recv across a duplex pair
- `disconnect_terminates_tasks` — `disconnect()` cleans up; remote sees `ConnectionBroke`
- `eof_emits_connection_broke` — peer closure surfaces `ConnectionBroke`
- `loopback_short_circuits` — sending to own host id uses the loopback channel, not the wire

Run: `cargo test -p collab_mode io_channel`

### `webchannel` — WebRTC-based message channel
Files: `collab_mode/src/webchannel.rs` (inline `mod tests`), `collab_mode/src/webchannel/e2e_tests.rs`

In-module tests (mostly `TestWebChannel` plumbing):
- `test_sync_send_instant`
- `test_sync_send_ms_zero`
- `test_send_to_non_deliverable_fails`
- `test_sync_broadcast`

E2E tests (real WebRTC stack with a signaling server):
- `test_basic_connection`
- `test_large_message_chunking`
- `test_multiple_concurrent_connections`
- `test_connection_failure_recovery` *(ignored — `todo!()`, needs rewrite for the new `SignalingClient`-based API)*
- `test_certificate_validation`
- `test_high_throughput`
- `test_ice_progress_messages`

Run: `cargo test -p collab_mode webchannel`
E2E only: `cargo test -p collab_mode webchannel::e2e_tests`

### `ssh_channel` — SshMsgChannel (per-peer channel over an ssh subprocess)
File: `collab_mode/src/ssh_channel/tests.rs`

- `local_subprocess_spawner_roundtrip` — spawns `cat` via `LocalSubprocessSpawner`, sends a Hey through the framed protocol, asserts it echoes back, then asserts disconnect kills the child
- `openssh_localhost_roundtrip` *(ignored — needs `ssh yuan@localhost` to work without a prompt)*

Run: `cargo test -p collab_mode ssh_channel`

### `signaling` — signaling-server client
File: `collab_mode/src/signaling/client_new_tests.rs`

- `test_signaling_client_bind`
- `test_signaling_client_create_sock`
- `test_sock_sdp_exchange`
- `test_sock_candidate_exchange`
- `test_error_message_handling`
- `test_connection_deduplication`
- `test_signaling_client_send`

Run: `cargo test -p collab_mode signaling`

### `server` — top-level server (`src/server.rs`) integration tests
Files: `collab_mode/src/server/tests/*.rs` (one file per request type or feature)

`tests/mod.rs` provides `TestEnvironment`, `TestChannelFactory`, `setup_hub_and_spoke_servers`, `MockEditor`, etc. All tests are `#[tokio::test]` and use the in-memory `TestWebChannel` factory unless they explicitly need the real signaling server.

| File | Tests |
|------|-------|
| `config.rs` | `test_expand_project_paths_home_directory`, `test_expand_project_paths_relative_error`, `test_declare_projects_relative_path_error`, `test_server_run_config_projects_expansion` |
| `connection.rs` | `test_accept_connect` |
| `delete_file.rs` | `test_delete_file`, `test_delete_file_permission_denied` |
| `doc_project.rs` | `test_doc_project_handling` |
| `list_files.rs` | `test_list_files_project_directory`, `test_list_files_from_remote`, `test_list_files_project_not_found`, `test_list_files_not_directory`, `test_list_files_empty_directory`, `test_list_files_nested_structure`, `test_list_files_sorted_alphanumerically` |
| `list_projects.rs` | `test_list_files_top_level` |
| `local_remote_ops.rs` | `test_local_ops_sent_to_remote_subscribers`, `test_bidirectional_ops_hub_owned_file` |
| `open_file.rs` | `test_open_file_basic`, `test_open_file_from_remote`, `test_open_file_create_mode`, `test_open_file_not_found`, `test_open_file_bad_request`, `test_open_file_already_open`, `test_open_file_doc_id_not_found`, `test_open_binary_file_rejected`, `test_create_file_permission_denied` |
| `send_info.rs` | `test_send_info_local_file`, `test_send_info_remote_file`, `test_send_info_multiple_subscribers` |
| `send_ops.rs` | `test_send_ops_e2e`, `test_send_ops_permission_denied` |
| `share_file.rs` | `test_share_file_e2e` |
| `shared_file_ops.rs` | `test_server_opens_own_shared_file`, `test_server_opens_own_file_before_remote`, `test_move_file_permission_denied`, `test_save_file_permission_denied` |
| `signaling_tests.rs` | `test_sdp_and_ice_candidate_routing`, `test_connect_message_routing` |
| `undo.rs` | `test_undo_e2e` |

Run: `cargo test -p collab_mode server::tests`
A single file: `cargo test -p collab_mode server::tests::open_file`
A single test: `cargo test -p collab_mode test_undo_e2e`

### `server` — transcript-driven OT tests
File: `collab_mode/src/server/transcript_tests.rs`
Transcripts: `collab_mode/src/server/transcripts/*.txt` (27 files)

A single discovery test (`test_all_transcripts`, multi-thread, 16 workers) runs every `*.txt` file in the transcripts directory through the mock-editor harness. Format is documented in `transcripts/README.md`; CLI usage in `transcripts/CLI_USAGE.md`.

Transcripts cover: `simple_insert`, `test_single_insert`, `test_two_editors`, `test_simple_undo`, `insert_delete`, `concurrent_edits`, `concurrent_undo`, `concurrent_delete_insert`, `three_way_concurrent`, `same_position_conflict`, `overlapping_deletes`, `paragraph_collaboration`, `multi_paragraph_edit`, `boundary_conflict_intensive`, `cascading_undo_redo_chaos`, `complex_interleaved`, `complex_undo_redo_long`, `comprehensive_stress_test`, `delete_insert_undo_marathon`, `delete_with_undo`, `edge_case_positions`, `position_tracking_stress`, `redo_chain`, `undo_grouped_replace`, `undo_redo`, `undo_redo_invalidation_storm`, `undo_with_concurrent_edit`.

Run all: `cargo test -p collab_mode test_all_transcripts`
Single transcript: `COLLAB_TRANSCRIPT=simple_insert cargo test -p collab_mode test_all_transcripts`
Helper script (handles env vars and `--print-result`/`--print-log`):
```sh
./collab_mode/src/server/tests/run_transcript list
./collab_mode/src/server/tests/run_transcript all
./collab_mode/src/server/tests/run_transcript run simple_insert
```

## Emacs Lisp tests

### ERT unit tests — `lisp/collab-mode-tests.el`
28 tests for filename and file-desc helpers:
- `collab--file-desc-parent/*` — 6 cases (project-root, standalone-buffer, file-in-root, file-in-subdirectory, deep-nested-file, directory-with-trailing-slash)
- `collab--parse-filename/*` — 8 cases (basic-file, project-root, nested-file, standalone-buffer, invalid-path, with-trailing-slash, with-spaces, special-characters)
- `collab--encode-filename/*` — 6 cases (mirroring parse cases)
- `collab--filename-round-trip/*` — 3 cases (basic, project-root, various-paths)
- `collab--file-desc-parent-encode`, `collab--parse-parent-encode`, `collab--parent-chain`
- `collab--push-with-limit/*` — 2 cases (basic, edge-cases)

Run (batch):
```sh
emacs -Q -batch -L lisp -l lisp/collab-mode-tests.el -f ert-run-tests-batch-and-exit
```

### Edit-tracking transcript tests — `lisp/edit-tracking-test.el` + `lisp/transcripts/*.transcript`
Runs commands from a transcript through `collab-test-mode` against a work buffer and a mirror buffer, then verifies the two are identical. Six transcripts:
- `test-simple.transcript`
- `test-complex.transcript`
- `test-navigation.transcript`
- `test-c-mode.transcript`
- `test-aggressive-indent.transcript`
- `test-aggressive-indent-hand-crafted.transcript`

Run interactively in Emacs: `M-x collab-test-run-transcript` or `M-x collab-test-run-all`.

### Standalone test runner — `test/run-tests.sh`
Wrapper that loads `record-change-test.el` (note: this file is not present in `lisp/`; the runner is currently broken — it expects `lisp/record-change-test.el` which appears to have been renamed to `edit-tracking-test.el`). Intended usage:
```sh
test/run-tests.sh                        # all transcripts in test/transcripts
test/run-tests.sh <transcript-path>      # single transcript
```

### Manual REPL scratch — `lisp/tests.el`
Not an automated test file. It is a scratch buffer of `should` expressions intended to be evaluated by hand against a running collab-mode session (`alice` host, signaling server). Runs `collab--connect-process`, then exercises `list-files-req`, `share-file-req`, `declare-project-req`, `parse-filename`, etc.

### `test/test-basic.el`
Small ERT smoke test that calls `collab-test-run-transcript` on `transcripts/test-simple.transcript`. Depends on the missing `record-change-test.el` and on a `test/transcripts/` directory that does not exist; not currently runnable.

## Binaries (manual / interactive testing)

`collab_mode/src/bin/` has two manual harnesses (built by `cargo build`, not run by `cargo test`):
- `test-editor-receptor` — exercises the editor-side socket protocol
- `test-websocket-receptor` — exercises the websocket receptor

And `collab-signal` is the signaling-server binary used by Makefile targets `signaling`, `session-1`, `session-2`.

## Ignored tests (`#[ignore]`)

`cargo test` skips these by default. They run via `make -C collab_mode test-ignored` (which is `cargo test -- --ignored --show-output`) or by passing `--ignored` directly.

- `ssh_channel::tests::openssh_localhost_roundtrip` — exercises the real `OpensshSpawner` path. Requires `ssh yuan@localhost` to succeed non-interactively (key-based auth, `cat` available on the remote).
  - Run: `cargo test -p collab_mode ssh_channel openssh_localhost -- --ignored`
- `webchannel::e2e_tests::e2e_tests::test_connection_failure_recovery` — currently `todo!()`; left as a reminder that the connection-recovery test needs rewriting against the new `SignalingClient` API.
  - Run: `cargo test -p collab_mode test_connection_failure_recovery -- --ignored` (will panic with the `todo!`).

Run all ignored tests at once:
```sh
make -C collab_mode test-ignored
# Equivalent: cargo test -- --ignored --show-output
```
Run both ignored and non-ignored: `cargo test -- --include-ignored`.

## Quick reference

| Want to run… | Command |
|---|---|
| Everything | `make -C collab_mode test` |
| One module | `cargo test -p collab_mode <module>` (e.g. `io_channel`, `engine`, `webchannel`, `signaling`, `server::tests`) |
| One test file | `cargo test -p collab_mode server::tests::open_file` |
| One test by name | `cargo test -p collab_mode test_undo_e2e` |
| With logs | `cargo test -p collab_mode <filter> -- --nocapture` |
| All transcripts | `./collab_mode/src/server/tests/run_transcript all` |
| One transcript | `./collab_mode/src/server/tests/run_transcript run <name>` |
| Ignored tests | `make -C collab_mode test-ignored` |
| ERT tests | `emacs -Q -batch -L lisp -l lisp/collab-mode-tests.el -f ert-run-tests-batch-and-exit` |
