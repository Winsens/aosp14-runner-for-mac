# make        build static musl aosp14-shim + build and sign host runner
# make host   build + ad-hoc sign the host runner (Virtualization.framework entitlement)
# make shim   build static aarch64-unknown-linux-musl init shim -> assets/aosp14-shim
# make test   host unit tests
BIN := target/release/aosp14-runner
SHIM_TARGET := aarch64-unknown-linux-musl
SHIM_OUT := assets/aosp14-shim

.PHONY: all host shim test clean
all: shim host

shim:
	@mkdir -p assets
	@if command -v rustup >/dev/null 2>&1; then \
		rustup target list --installed --toolchain stable | grep -qx "$(SHIM_TARGET)" || rustup target add --toolchain stable "$(SHIM_TARGET)"; \
		TC_BIN=$$(dirname "$$(rustup which --toolchain stable rustc)"); \
		(cd guest/aosp14-shim && PATH="$${TC_BIN}:$${PATH}" RUSTC="$${TC_BIN}/rustc" "$${TC_BIN}/cargo" build --release --target "$(SHIM_TARGET)"); \
	else \
		(cd guest/aosp14-shim && cargo build --release --target "$(SHIM_TARGET)"); \
	fi
	cp guest/aosp14-shim/target/$(SHIM_TARGET)/release/aosp14-shim $(SHIM_OUT)
	chmod 0755 $(SHIM_OUT)

host:
	cargo build --release
	codesign --force --sign - --entitlements entitlements.plist $(BIN)

test:
	cargo test

clean:
	cargo clean
	cd guest/aosp14-shim && cargo clean
	rm -f $(SHIM_OUT)
