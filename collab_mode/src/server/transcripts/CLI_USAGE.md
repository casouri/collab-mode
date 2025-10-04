# Transcript Test Runner

A bash script for managing and running transcript tests.

## Location

The script is located at: `collab_mode/src/server/tests/run_transcript`

## Commands

### List all transcripts
```bash
./collab_mode/src/server/tests/run_transcript list
```
Shows all available transcript test files with their names.

### Run all tests
```bash
./collab_mode/src/server/tests/run_transcript all
```
Runs all transcript tests using cargo test.

### Run a single test
```bash
./collab_mode/src/server/tests/run_transcript run <filename>
# Examples:
./collab_mode/src/server/tests/run_transcript run simple_insert
./collab_mode/src/server/tests/run_transcript run concurrent_edits.txt
```
Runs a specific transcript test (with or without .txt extension).

### Show help
```bash
./collab_mode/src/server/tests/run_transcript --help
```
Display usage information.

## Quick Examples

```bash
# See what tests are available
./collab_mode/src/server/tests/run_transcript list

# Run all transcript tests
./collab_mode/src/server/tests/run_transcript all

# Run a specific test
./collab_mode/src/server/tests/run_transcript run simple_insert
```

## Direct cargo test usage

You can also run tests directly with cargo:

```bash
# Run all transcript tests
cargo test test_all_transcripts

# Run a specific transcript by setting COLLAB_TRANSCRIPT env var
COLLAB_TRANSCRIPT=simple_insert cargo test test_all_transcripts
```

## Exit Codes

- 0: Success (test passed)
- 1: Failure (test failed or error occurred)
