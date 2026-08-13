PREFIX ?= $(HOME)/.local
UNAME_S := $(shell uname -s)

.PHONY: build install test lint clean deb app

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

install: build
	mkdir -p $(PREFIX)/bin
	install -m 755 target/release/devdock $(PREFIX)/bin/devdock
ifeq ($(UNAME_S),Linux)
	mkdir -p $(PREFIX)/share/applications $(PREFIX)/share/icons/hicolor/256x256/apps
	install -m 644 assets/devdock.desktop $(PREFIX)/share/applications/devdock.desktop
	install -m 644 assets/icons/git-manage-256.png $(PREFIX)/share/icons/hicolor/256x256/apps/devdock.png
endif
	@echo "Installed to $(PREFIX)/bin/devdock"

clean:
	cargo clean

# Build a Debian package (requires: cargo install cargo-deb)
deb:
	cargo deb

# Build DevDock.app (macOS bundle with icon and proper dock name)
app: build
	rm -rf dist/DevDock.app
	mkdir -p dist/DevDock.app/Contents/MacOS dist/DevDock.app/Contents/Resources
	cp packaging/Info.plist dist/DevDock.app/Contents/
	cp target/release/devdock dist/DevDock.app/Contents/MacOS/
	sh packaging/make-icns.sh assets/icons/git-manage-256.png dist/DevDock.app/Contents/Resources/DevDock.icns
	@echo "Built dist/DevDock.app — drag it to /Applications"
