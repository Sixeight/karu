PREFIX ?= $(HOME)/.local

.PHONY: install uninstall test

install:
	cargo build --release
	install -d $(PREFIX)/bin
	install -m 755 target/release/karu $(PREFIX)/bin/karu
	install -m 755 target/release/git-karu $(PREFIX)/bin/git-karu

uninstall:
	rm -f $(PREFIX)/bin/karu $(PREFIX)/bin/git-karu

test:
	cargo test
