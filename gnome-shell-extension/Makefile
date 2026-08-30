UUID := voxkey@hy26v.github.io
DEST := $(HOME)/.local/share/gnome-shell/extensions/$(UUID)

.PHONY: install uninstall

install:
	@mkdir -p $(DEST)
	@install -m 644 metadata.json $(DEST)/metadata.json
	@install -m 644 extension.js $(DEST)/extension.js
	@install -m 644 constants.js $(DEST)/constants.js
	@install -m 644 util.js $(DEST)/util.js
	@install -m 644 capsule.js $(DEST)/capsule.js
	@install -m 644 toggle.js $(DEST)/toggle.js
	@install -m 644 stylesheet.css $(DEST)/stylesheet.css
	@echo "Installed to $(DEST)"
	@echo "Enable with: gnome-extensions enable $(UUID)"
	@echo "After a shell restart (Wayland: log out and back in)."

uninstall:
	@rm -rf $(DEST)
	@echo "Removed $(DEST)"
