.PHONY: install linux macos

# `linux` and `macos` are aliases: the install steps are identical on both, and
# separate recipes only imply a difference that does not exist.
linux: install
macos: install

install:
	cargo install --path . --locked --jobs 4
	install -d "$(HOME)/.config/super-herdr"
	@if test -e "$(HOME)/.config/super-herdr/config.toml"; then \
		echo "Keeping existing $(HOME)/.config/super-herdr/config.toml"; \
	else \
		echo "No targets configured yet"; \
		echo "Run: super-herdr target add NAME --ssh SSH_ALIAS --discover-sessions"; \
	fi
	@echo "Run: super-herdr clipboard check"
	@echo "Then: super-herdr probe"
