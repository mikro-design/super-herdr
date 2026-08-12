.PHONY: macos macos-config

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

macos-config:
	install -d "$(HOME)/.config/super-herdr"
	@if test -e "$(HOME)/.config/super-herdr/config.toml"; then \
		cp -p "$(HOME)/.config/super-herdr/config.toml" "$(HOME)/.config/super-herdr/config.toml.backup"; \
		echo "Backed up existing config.toml to config.toml.backup"; \
	fi
	install -m 600 config.macos.example.toml "$(HOME)/.config/super-herdr/config.toml"
	@echo "Installed current macOS configuration"
