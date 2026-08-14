BUILD_DIR ?= build
PROFILE ?= development
PREFIX ?= /usr/local
APP_ID := io.github.srwalkerb.DuckPackages
BINARY := $(BUILD_DIR)/src/duck-packages

DESKTOP_FILE := data/$(APP_ID).desktop
METAINFO_FILE := data/$(APP_ID).metainfo.xml
PO_FILE := po/pt_BR.po

.PHONY: all
all: build

.PHONY: setup
setup:
	meson setup $(BUILD_DIR) -Dprofile=$(PROFILE)

.PHONY: configure
configure:
	meson configure $(BUILD_DIR) -Dprofile=$(PROFILE)

.PHONY: build
build:
	meson compile -C $(BUILD_DIR)

.PHONY: run
run: build
	$(BINARY)

.PHONY: test
test:
	cargo test

.PHONY: fmt
fmt:
	cargo fmt

.PHONY: fmt-check
fmt-check:
	cargo fmt --check

.PHONY: clippy
clippy:
	cargo clippy --all-targets -- -D warnings

.PHONY: check
check: fmt-check clippy test validate

.PHONY: validate
validate:
	desktop-file-validate $(DESKTOP_FILE)
	appstreamcli validate --no-net $(METAINFO_FILE)
	msgfmt --check --check-accelerators=_ -o /dev/null $(PO_FILE)

.PHONY: rpm
rpm:
	./scripts/build-rpm.sh

.PHONY: install
install:
	meson install -C $(BUILD_DIR) --prefix $(PREFIX)

.PHONY: dist
dist: rpm

.PHONY: clean
clean:
	meson compile -C $(BUILD_DIR) --clean 2>/dev/null || true
	rm -rf $(BUILD_DIR) target

.PHONY: distclean
distclean: clean
	rm -rf packaging/rpmbuild .build-deps
