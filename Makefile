PREFIX ?= $(HOME)/.local
UNAME_S := $(shell uname -s)

.PHONY: build install test lint clean

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

install: build
	mkdir -p $(PREFIX)/bin
	install -m 755 target/release/git-manage $(PREFIX)/bin/git-manage
ifeq ($(UNAME_S),Linux)
	mkdir -p $(PREFIX)/share/applications
	install -m 644 assets/git-manage.desktop $(PREFIX)/share/applications/git-manage.desktop
endif
	@echo "Installed to $(PREFIX)/bin/git-manage"

clean:
	cargo clean
