# storycast — one-command operations.
#
#   make build                  compile the workspace
#   make tui                    live cluster dashboard (needs the inductor up)
#   make serve                  run the inductor (START=1 COUNT=100 by default)
#   make agent                  run a local worker (needs the inductor up)
#   make provision ADDR=<ip>    onboard a machine by address (one-shot)
#   make provision BOX=<name>    onboard a linked machine (see `link` below)
#   make link NAME=<n> ADDR=<ip>  remember a machine in .bm/machines.json
#   make test                   full test suite + clippy
#
# Variables (override with `make tui API=http://box:8901`):
#   API    inductor base URL            (default http://127.0.0.1:8901)
#   START  first chapter for serve      (default 1)
#   COUNT  how many chapters            (default 100)
#   BOX    linked box name for provision (default box-1 when linked)

RUST_DIR := rust
BIN := $(RUST_DIR)/target/debug
API ?= http://127.0.0.1:8901
START ?= 1
COUNT ?= 100

.PHONY: build tui serve agent provision link test

build:
	cargo build --workspace --manifest-path $(RUST_DIR)/Cargo.toml

tui: build
	$(BIN)/bm-inductor tui --api $(API)

serve: build
	$(BIN)/bm-inductor serve --bind 0.0.0.0 --port 8901 --start $(START) --count $(COUNT)

agent: build
	$(BIN)/bm-agent worker --inductor $(API)

provision: build
ifdef ADDR
	$(BIN)/bm-inductor provision --addr $(ADDR) --user thang
else
	$(BIN)/bm-inductor provision --box $(BOX)
endif

link: build
ifndef NAME
	$(error NAME and ADDR are required: make link NAME=box-1 ADDR=192.168.2.2)
endif
ifndef ADDR
	$(error NAME and ADDR are required: make link NAME=box-1 ADDR=192.168.2.2)
endif
	$(BIN)/bm-inductor link --name $(NAME) --addr $(ADDR) --user thang $(if $(KEY),--key $(KEY))

BOX ?= box-1
KEY ?=

test:
	cargo test --workspace --manifest-path $(RUST_DIR)/Cargo.toml
	cargo clippy --all-targets --manifest-path $(RUST_DIR)/Cargo.toml -- -D warnings
