# make run [ARGS="-p prod -r us-west-1"]   build and launch
# make install                             a `s9s` launcher in ~/.local/bin (see install.sh)
# make test

CARGO ?= $(shell command -v cargo || ls $$HOME/.cargo/bin/cargo /opt/homebrew/opt/rustup/bin/cargo /usr/local/opt/rustup/bin/cargo 2>/dev/null | head -n 1)
# Rustup's proxies find rustc through PATH.
export PATH := $(dir $(CARGO)):$(PATH)

.PHONY: run install uninstall test

run:
	$(CARGO) run --release --locked -- $(ARGS)

install:
	./install.sh

uninstall:
	./install.sh --uninstall

test:
	$(CARGO) test --locked
