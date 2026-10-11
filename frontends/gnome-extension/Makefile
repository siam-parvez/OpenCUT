# SPDX-License-Identifier: GPL-2.0-or-later
#
# OpenCUT GNOME Shell extension.
#   make            compile the GSettings schema (in place, for local testing)
#   make test       unit tests for lib.js under Node and GJS
#   make check      syntax-check every module + validate the schema
#   make lint       ESLint (needs: npm i -D eslint@9 @eslint/js@9 globals)
#   make zip        build dist/opencut@opencut.github.io.shell-extension.zip
#   make install    install the zip for the current user (does NOT enable it)
#   make uninstall  remove it for the current user
#   make smoke      start a throwaway headless gnome-shell with the extension

UUID    := opencut@opencut.github.io
SRC     := $(UUID)
DIST    := dist
ZIP     := $(DIST)/$(UUID).shell-extension.zip
SCHEMA  := $(SRC)/schemas/org.gnome.shell.extensions.opencut.gschema.xml
MODULES := $(SRC)/extension.js $(SRC)/prefs.js $(SRC)/lib.js
NODE    ?= node
GJS     ?= gjs

.PHONY: all schemas test test-node test-gjs check lint zip install uninstall smoke clean

all: schemas

schemas: $(SRC)/schemas/gschemas.compiled

$(SRC)/schemas/gschemas.compiled: $(SCHEMA)
	glib-compile-schemas --strict $(SRC)/schemas

test: test-node test-gjs

test-node:
	$(NODE) --test tests/*.test.mjs

test-gjs:
	@if command -v $(GJS) >/dev/null 2>&1; then $(GJS) -m tests/lib.test.mjs; \
	else echo "gjs not found; skipping GJS run"; fi

# Node can only parse (not run) the shell modules: gi:// and resource:// imports
# exist only inside gnome-shell. Copies get a .mjs suffix so Node parses ESM.
check: schemas
	@tmp=$$(mktemp -d) && trap 'rm -rf "$$tmp"' EXIT && \
	for f in $(MODULES); do \
		cp "$$f" "$$tmp/$$(basename $$f .js).mjs" && \
		$(NODE) --check "$$tmp/$$(basename $$f .js).mjs" && echo "parse ok: $$f" || exit 1; \
	done
	glib-compile-schemas --strict --dry-run $(SRC)/schemas && echo "schema ok"
	@$(NODE) -e 'JSON.parse(require("fs").readFileSync("$(SRC)/metadata.json","utf8")); console.log("metadata ok")'

ESLINT ?= npx --no-install eslint

lint:
	$(ESLINT) --max-warnings=0 .

# gnome-extensions pack compiles the schema into the zip itself.
zip: $(ZIP)

$(ZIP): $(MODULES) $(SRC)/metadata.json $(SRC)/stylesheet.css $(SCHEMA) $(SRC)/icons/opencut-symbolic.svg $(SRC)/LICENSE
	@mkdir -p $(DIST)
	gnome-extensions pack $(SRC) --force --out-dir=$(DIST) \
		--extra-source=lib.js --extra-source=icons --extra-source=LICENSE
	@echo "built $(ZIP)"

install: zip
	gnome-extensions install --force $(ZIP)
	@echo "Installed. Log out and back in (Wayland), then: gnome-extensions enable $(UUID)"

uninstall:
	gnome-extensions uninstall $(UUID)

smoke: zip
	./tests/smoke/run-headless.sh $(ZIP)

clean:
	rm -rf $(DIST) $(SRC)/schemas/gschemas.compiled
