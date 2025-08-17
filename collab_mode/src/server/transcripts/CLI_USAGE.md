# Transcript Test CLI Tool

A command-line tool for managing, viewing, and running transcript tests.

## Installation

The tool is automatically built with the project:
```bash
# Build without test runner (view-only)
cargo build --bin transcript-runner

# Build with test runner capability
cargo build --bin transcript-runner --features test-runner
```

## Commands

### List all transcripts
```bash
cargo run --bin transcript-runner list
```
Shows all available transcript test files with their names.

### View a transcript
```bash
cargo run --bin transcript-runner show <filename>
# Examples:
cargo run --bin transcript-runner show simple_insert
cargo run --bin transcript-runner show simple_insert.txt
```
Displays the full content of a transcript test file.

### Run a single test
```bash
cargo run --bin transcript-runner --features test-runner test <filename>
# Examples:
cargo run --bin transcript-runner --features test-runner test simple_insert
cargo run --bin transcript-runner --features test-runner test concurrent_edits.txt
```
Runs a specific transcript test and reports pass/fail status.

### Run all tests
```bash
cargo test --lib server::transcript_tests::test_all_transcripts
```
Runs all transcript tests using the standard test harness.

## Quick Examples

```bash
# See what tests are available
cargo run --bin transcript-runner list

# Look at a specific test
cargo run --bin transcript-runner show concurrent_edits

# Run a specific test
cargo run --bin transcript-runner --features test-runner test simple_insert

# Run all transcript tests
cargo test --lib server::transcript_tests
```

## Exit Codes

- 0: Success (test passed)
- 1: Failure (test failed or error occurred)