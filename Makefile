.POSIX:

.PHONY: test test-all session-1 session-2

LOG_ENV=RUST_LOG=debug

RUST_ENV=RUST_BACKTRACE=1 $(LOG_ENV)

test:
	cargo test

test-ignored:
	cargo test -- --ignored --show-output

session-1:
	$(RUST_ENV) cargo run --bin collab-mode -- run --socket

session-2:
	$(RUST_ENV) cargo run --bin collab-mode -- run --socket --socket-port 7702

signaling:
	$(RUST_ENV) cargo run --bin collab-signal -- run

doc:
	cargo doc --document-private-items --open
