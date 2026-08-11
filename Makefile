PREFIX ?= $(HOME)/.local

.PHONY: build install test lint clean

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

install: build
	install -Dm755 target/release/git-manage $(PREFIX)/bin/git-manage
	install -Dm644 assets/git-manage.desktop $(PREFIX)/share/applications/git-manage.desktop

clean:
	cargo clean
