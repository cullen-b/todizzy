APP_NAME   := Todizzy
BINARY     := todizzy
BUNDLE_DIR := target/release/$(APP_NAME).app
CONTENTS   := $(BUNDLE_DIR)/Contents

.PHONY: all build bundle run clean

all: bundle

build:
	cargo build --release

bundle: build
	@echo "→ Creating .app bundle …"
	@mkdir -p $(CONTENTS)/MacOS
	@mkdir -p $(CONTENTS)/Resources
	@cp target/release/$(BINARY) $(CONTENTS)/MacOS/$(BINARY)
	@cp Info.plist $(CONTENTS)/Info.plist
	@echo "✓ Bundle: $(BUNDLE_DIR)"

run: bundle
	@open $(BUNDLE_DIR)

# Rebuild & relaunch (kill the previous instance first)
dev: bundle
	@pkill -f $(BINARY) 2>/dev/null || true
	@sleep 0.1
	@open $(BUNDLE_DIR)

clean:
	cargo clean
	rm -rf $(BUNDLE_DIR)

# Quick debug run without bundling (no LSUIElement, dock icon shows)
run-debug:
	cargo run
