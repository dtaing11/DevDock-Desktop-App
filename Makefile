PREFIX ?= $(HOME)/.local
UNAME_S := $(shell uname -s)

.PHONY: build install test lint clean deb

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
	mkdir -p $(PREFIX)/share/applications $(PREFIX)/share/icons/hicolor/256x256/apps
	install -m 644 assets/git-manage.desktop $(PREFIX)/share/applications/git-manage.desktop
	install -m 644 assets/icons/git-manage-256.png $(PREFIX)/share/icons/hicolor/256x256/apps/git-manage.png
endif
	@echo "Installed to $(PREFIX)/bin/git-manage"

clean:
	cargo clean

# Build a Debian package (requires: cargo install cargo-deb)
deb:
	cargo deb
