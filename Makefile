# beyond-myriads-converter — one-command operations.
#
#   make build                  compile the workspace
#   make tui                    live cluster dashboard (needs the inductor up)
#   make serve                  run the inductor (START=1 COUNT=100 by default)
#   make agent                  run a local worker (needs the inductor up)
#   make provision ADDR=<ip>    onboard a machine by address
#   make test                   full test suite + clippy
#
# Variables (override with `make tui API=http://box:8901`):
#   API    inductor base URL            (default http://127.0.0.1:8901)
#   START  first chapter for serve      (default 1)
#   COUNT  how many chapters            (default 100)

RUST_DIR := rust
BIN := $(RUST_DIR)/target/debug
API ?= http://127.0.0.1:8901
START ?= 1
COUNT ?= 100

.PHONY: build tui serve agent provision test

build:
	cargo build --workspace --manifest-path $(RUST_DIR)/Cargo.toml

tui: build
	$(BIN)/bm-inductor tui --api $(API)

serve: build
	$(BIN)/bm-inductor serve --bind 0.0.0.0 --port 8901 --start $(START) --count $(COUNT)

agent: build
	$(BIN)/bm-agent worker --inductor $(API)

provision: build
ifndef ADDR
	$(error ADDR is required: make provision ADDR=192.168.2.2)
endif
	$(BIN)/bm-inductor provision --addr $(ADDR) --user thang --key ~/.ssh/ssh-key-my-wsl

test:
	cargo test --workspace --manifest-path $(RUST_DIR)/Cargo.toml
	cargo clippy --all-targets --manifest-path $(RUST_DIR)/Cargo.toml -- -D warnings
