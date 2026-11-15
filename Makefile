# Builds LightningRouterRS.xcframework, the Rust half of the app. Xcode links
# whatever copy is on disk, so run this from the repo root after changing
# src/ or frame/, then build the app in Xcode.
#
#   make targets   install the Rust targets (once)
#   make           build the xcframework and bundle.zip
#   make test      run every crate's tests on this Mac

CARGO ?= cargo

LIB := liblightning_router.a
SIM_TARGETS := aarch64-apple-ios-sim x86_64-apple-ios
MAC_TARGETS := aarch64-apple-darwin x86_64-apple-darwin
TARGETS := aarch64-apple-ios $(SIM_TARGETS) $(MAC_TARGETS)

.PHONY: all build targets test clean

all: build

targets:
	rustup target add $(TARGETS)

build:
	$(CARGO) build --lib --release $(addprefix --target ,$(TARGETS))
	rm -rf build LightningRouterRS.xcframework bundle.zip
	mkdir -p build/include
	cp lightning_router_rs.h module.modulemap build/include
	lipo -create -output build/liblightning_router-ios-sim.a \
		$(foreach target,$(SIM_TARGETS),target/$(target)/release/$(LIB))
	lipo -create -output build/liblightning_router-macos.a \
		$(foreach target,$(MAC_TARGETS),target/$(target)/release/$(LIB))
	xcodebuild -create-xcframework \
		-library target/aarch64-apple-ios/release/$(LIB) -headers build/include \
		-library build/liblightning_router-ios-sim.a -headers build/include \
		-library build/liblightning_router-macos.a -headers build/include \
		-output LightningRouterRS.xcframework
	zip -qr bundle.zip LightningRouterRS.xcframework
	openssl dgst -sha256 bundle.zip

test:
	$(CARGO) test
	cd frame && $(CARGO) test
	cd relay && $(CARGO) test

clean:
	$(CARGO) clean
	rm -rf build LightningRouterRS.xcframework bundle.zip lightning_router_rs.h
