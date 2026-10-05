# bm-video — the plugin's own operations.
#
# Nothing here is part of the pipeline, and the pipeline's `make test` does not
# reach it: the plugin is a separate workspace (see Cargo.toml), built and
# checked from this directory.
#
#   make build              debug binary
#   make release            optimized binary (the one to render with)
#   make test               the plugin's own gate: cargo test + clippy -D warnings
#   make run ARGS="…"       build --release, then run with ARGS
#   make clean
#
# `cargo` is not always on PATH — a rustup shim lives in ~/.cargo/bin, which a
# non-login shell may not have.

CARGO := $(shell command -v cargo 2>/dev/null || echo $(HOME)/.cargo/bin/cargo)

.PHONY: build release test run clean

build:
	$(CARGO) build

release:
	$(CARGO) build --release

test:
	$(CARGO) test
	$(CARGO) clippy --all-targets -- -D warnings

run: release
	./target/release/bm-video $(ARGS)

clean:
	$(CARGO) clean
