.PHONY: linux macos

linux:
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

macos:
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
