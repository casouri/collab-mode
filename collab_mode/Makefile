.POSIX:

.PHONY: test test-slow session-1 session-2 upload

LOG_ENV=RUST_LOG=debug

RUST_ENV=RUST_BACKTRACE=1 $(LOG_ENV)

test:
	cargo test -- --test-threads=1 # Avoid too many open files, port alrady used, etc.

test-slow:
	cargo test -- --ignored --test-threads=1 --show-output

session-1:
	$(RUST_ENV) cargo run --bin collab_mode -- run --socket --socket-port 7701 --config ../fixture/Alice/config

session-2:
	$(RUST_ENV) cargo run --bin collab_mode -- run --socket --socket-port 7702 --config ../fixture/Bob/config

signaling:
	$(RUST_ENV) cargo run --bin collab-signal -- run --port 6001

doc:
	cargo doc --document-private-items --open

emacs-2:
	~/emacs-head/src/emacs --eval "(setq collab-connection-type '(socket 7702))"

upload:
	rsync -rP ~/p/collab-mode yuan@collab-mode: --exclude "target" --exclude ".git" --exclude "release"

# To deploy new signal version:
# ssh root@collab-mode
# cd /home/yuan/collab-mode
# cargo build --bin collab-signal
# systemctl restart collab-signal
