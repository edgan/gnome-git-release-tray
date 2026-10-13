PREFIX ?= $(HOME)/.local
BINDIR = $(PREFIX)/bin
DATADIR = $(PREFIX)/share

.PHONY: build install check
build:
	cargo build --release --locked

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test --locked

install: build
	install -Dm755 target/release/gnome-git-release-tray "$(DESTDIR)$(BINDIR)/gnome-git-release-tray"
	install -Dm644 data/io.github.gnome_git_release_tray.svg "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/io.github.gnome_git_release_tray.svg"
	install -Dm644 data/io.github.gnome_git_release_tray.png "$(DESTDIR)$(DATADIR)/icons/hicolor/200x200/apps/io.github.gnome_git_release_tray.png"
	install -d "$(DESTDIR)$(DATADIR)/applications"
	sed 's|^Exec=.*|Exec="$(BINDIR)/gnome-git-release-tray"|' data/io.github.gnome_git_release_tray.desktop > "$(DESTDIR)$(DATADIR)/applications/io.github.gnome_git_release_tray.desktop"
