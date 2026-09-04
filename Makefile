SHELL := /bin/sh

LINUX_TRIPLE := x86_64-unknown-linux-gnu
MONOREPO_ROOT := $(abspath ..)
RUNTIME_ROOT := $(MONOREPO_ROOT)/runtime
DOCKER_PLATFORM := linux/$(shell uname -m | sed 's/x86_64/amd64/')

.PHONY: check release release-linux

check:
	cargo fmt --all -- --check
	cargo test
	cargo clippy --all-targets -- -D warnings

release:
	cargo build --release --locked

release-linux:
ifeq ($(shell uname -s),Linux)
	cargo build --release --locked --target $(LINUX_TRIPLE)
else
	docker build -q --platform $(DOCKER_PLATFORM) -t tape-linux-builder -f $(RUNTIME_ROOT)/deploy/Dockerfile.linux-builder $(RUNTIME_ROOT)/deploy
	docker run --rm --platform $(DOCKER_PLATFORM) \
		-v $(MONOREPO_ROOT):/src -w /src/git \
		-v tape-cargo-registry:/usr/local/cargo/registry \
		-v $(HOME)/.cargo/git:/usr/local/cargo/git \
		-e CARGO_TARGET_DIR=/src/git/target/linux \
		-e CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
		-e CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
		tape-linux-builder \
		cargo build --release --locked --target $(LINUX_TRIPLE)
	@mkdir -p target/$(LINUX_TRIPLE)/release
	cp target/linux/$(LINUX_TRIPLE)/release/git-remote-tape target/$(LINUX_TRIPLE)/release/git-remote-tape
endif
