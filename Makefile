.PHONY: macos

macos:
	cargo install --path . --locked --jobs 4
	install -d "$(HOME)/.config/super-herdr"
	@if test -e "$(HOME)/.config/super-herdr/config.toml"; then \
		echo "Keeping existing $(HOME)/.config/super-herdr/config.toml"; \
	else \
		install -m 600 config.macos.example.toml "$(HOME)/.config/super-herdr/config.toml"; \
		echo "Installed $(HOME)/.config/super-herdr/config.toml"; \
	fi
	@echo "Run: super-herdr check"
	@echo "Then: super-herdr"
