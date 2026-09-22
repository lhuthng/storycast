# storycast — one-command operations.
#
#   make build                  compile the workspace
#   make tts                    cross-build the linux TTS sidecar + stage its runtime
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
# `cargo` is not always on PATH — a rustup shim lives in ~/.cargo/bin, which a
# non-login shell may not have. Fall back to it rather than failing obscurely.
CARGO := $(shell command -v cargo 2>/dev/null || echo $(HOME)/.cargo/bin/cargo)
# cargo's sibling tools live beside it; `cargo-zigbuild` shells out to `zig`, so
# both have to be findable on PATH for the build, not just for the check.
CARGO_BIN_DIR := $(patsubst %/,%,$(dir $(CARGO)))
ZIG := $(shell command -v zig 2>/dev/null || echo $(CARGO_BIN_DIR)/zig)
API ?= http://127.0.0.1:8901
START ?= 1
COUNT ?= 100

.PHONY: build build-inductor tui serve agent provision link test

build:
	$(CARGO) build --workspace --manifest-path $(RUST_DIR)/Cargo.toml

# The dashboard and the inductor never touch the TTS sidecar binary, so
# they must not wait on it: `bm-tts` links C++ system libraries, and a
# toolchain move (Xcode CLT clang 17 -> 21) breaks that link while the
# Rust crates are fine. Building only what these commands run keeps
# `make tui` working through it.
build-inductor:
	$(CARGO) build -p bm-inductor -p bm-agent --manifest-path $(RUST_DIR)/Cargo.toml

tui: build-inductor
	$(BIN)/bm-inductor tui --api $(API)

serve: build-inductor
	$(BIN)/bm-inductor serve --bind 0.0.0.0 --port 8901 --start $(START) --count $(COUNT)

agent: build-inductor
	$(BIN)/bm-agent worker --inductor $(API)

provision: build tts
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
	$(CARGO) test --workspace --manifest-path $(RUST_DIR)/Cargo.toml
	$(CARGO) clippy --all-targets --manifest-path $(RUST_DIR)/Cargo.toml -- -D warnings

# ── the TTS sidecar: its runtime, and its binary ────────────────────────────
#
# bm-tts is not built like the rest of the workspace. Two differences, both
# deliberate:
#
#   * It is a **release** cross-build. `agent_binary_for` ships the agent as a
#     debug build, but bm-tts's hot loop is a hand-written SIMD matvec — a debug
#     build would undo it. Hence `release` here.
#   * It links a **shared** ONNX Runtime rather than the one `ort-sys`
#     downloads. That is not a preference: `ort-sys` would otherwise
#     static-link a C++ archive, and `zig cc` is a C driver, so libstdc++ never
#     arrives and the link fails on `std::filesystem`. Both env vars below are
#     required — `ORT_LIB_LOCATION` alone is ignored.

ORT_VERSION := 1.30.0
# Pinned so the runtime is a verified artifact, not whatever the CDN serves
# today. Verified a drop-in for the pip wheel's library: same binary, same
# request, byte-identical wav.
ORT_TARBALL_SHA256 := a5ed5a3cac51fbb2e90da632ae43d19212faaa20e76484e62bcb7c23ddb3b3fd
ORT_URL := https://github.com/microsoft/onnxruntime/releases/download/v$(ORT_VERSION)/onnxruntime-linux-x64-$(ORT_VERSION).tgz
# Absolute: `ort-sys` resolves this relative to the *crate* directory, not to
# wherever make was invoked from, so a relative path fails with "Failed to read
# contents of … (does it exist?)".
ORT_DIR := $(CURDIR)/$(RUST_DIR)/target/ort-linux-x64
TTS_TARGET := x86_64-unknown-linux-gnu
TTS_BIN := $(RUST_DIR)/target/$(TTS_TARGET)/release/bm-tts

.PHONY: runtime tts

runtime:
	@mkdir -p "$(ORT_DIR)"
	@if [ -e "$(ORT_DIR)/libonnxruntime.so" ] && [ -e "$(ORT_DIR)/libonnxruntime.so.1" ]; then \
		echo "onnxruntime $(ORT_VERSION) already staged in $(ORT_DIR)"; \
	else \
		set -e; \
		echo "fetching onnxruntime $(ORT_VERSION)"; \
		cd "$(ORT_DIR)"; \
		curl -fsSL -o ort.tgz "$(ORT_URL)"; \
		echo "$(ORT_TARBALL_SHA256)  ort.tgz" | shasum -a 256 -c -; \
		tar xzf ort.tgz --strip-components=2 \
			onnxruntime-linux-x64-$(ORT_VERSION)/lib/libonnxruntime.so.$(ORT_VERSION) \
			onnxruntime-linux-x64-$(ORT_VERSION)/lib/libonnxruntime.so.1 \
			onnxruntime-linux-x64-$(ORT_VERSION)/lib/libonnxruntime.so; \
		rm -f ort.tgz; \
		echo "staged: $$(ls | tr '\n' ' ')"; \
	fi

tts: runtime
	@if [ ! -x "$(CARGO_BIN_DIR)/cargo-zigbuild" ] && ! command -v cargo-zigbuild >/dev/null 2>&1; then \
		echo "cargo-zigbuild is required: $(CARGO) install cargo-zigbuild"; exit 1; fi
	@[ -x "$(ZIG)" ] || { echo "zig is required (looked for $(ZIG)): https://ziglang.org/download"; exit 1; }
	PATH="$(CARGO_BIN_DIR):$$PATH" ORT_LIB_LOCATION="$(ORT_DIR)" ORT_PREFER_DYNAMIC_LINK=1 \
		$(CARGO) zigbuild --release --target $(TTS_TARGET) -p bm-tts --bin bm-tts \
		--manifest-path $(RUST_DIR)/Cargo.toml
	@ls -l "$(TTS_BIN)" | awk '{printf "  %d bytes  %s\n", $$5, $$NF}'
