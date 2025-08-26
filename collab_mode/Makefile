.POSIX:

.PHONY: test test-all session-1 session-2 upload

LOG_ENV=RUST_LOG=info

RUST_ENV=RUST_BACKTRACE=1 $(LOG_ENV)

test:
	cargo test

test-ignored:
	cargo test -- --ignored --show-output

session-1:
	$(RUST_ENV) cargo run --bin collab_mode -- run --socket --socket-port 7701 --config ../fixture --profile=Alice

session-2:
	$(RUST_ENV) cargo run --bin collab_mode -- run --socket --socket-port 7702 --config ../fixture --profile=Bob

signaling:
	$(RUST_ENV) cargo run --bin collab-signal -- run --port 6001

doc:
	cargo doc --document-private-items --open

run-2:
	$(RUST_ENV) cargo run --bin collab-mode -- run --socket --socket-port 7702 &
	~/emacs-head/src/emacs -f collab--setup-2

upload:
	rsync -rP ~/p/collab-mode yuan@collab-mode: --exclude "target" --exclude ".git" --exclude "release"
