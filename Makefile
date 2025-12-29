include makefiles/net.mk

SCCACHE := $(shell which sccache)
# https://github.com/tikv/jemallocator/issues/146
# >> force-frame-pointers is required for profiling, add ~1% CPU overhead
# >> allow-multiple-definition is required so we can replace system allocator for C projects, like openssl, ffmpeg, etc.
# These CFLAGS are needed to make sure everything compiles with frame-pointer otherwise segfault will happen.
export RUSTFLAGS := $(RUSTFLAGS) -C link-arg=-Wl,--allow-multiple-definition -C force-frame-pointers=yes
export CFLAGS := $(CFLAGS) -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer
export CXXFLAGS := $(CXXFLAGS) -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer
# ===

CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) RUSTFLAGS="$(RUSTFLAGS)" cargo
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

.PHONY: all help dev build release profile flamegraph perf deps brew-deps cargo-deps clean
all: build

dev:
	$(CARGO_CMD) run -p pulsebeam -- --dev

build:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

release:
	$(CARGO_CMD) build --verbose --release -p pulsebeam

profile: build

flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

perf: profile
	taskset -c 2-5 perf record --call-graph dwarf $(BINARY)
	hotspot perf.data

deps: brew-deps cargo-deps

brew-deps:
	brew install git-cliff axodotdev/tap/cargo-dist

cargo-deps:
	$(CARGO_CMD) install cargo-smart-release --features allow-emoji
	$(CARGO_CMD) install flamegraph cargo-machete

gh-deps:
	gh extension install yusukebe/gh-markdown-preview

preview-markdown:
	gh markdown-preview

clean:
	$(CARGO_CMD) clean
	rm -f perf.data flamegraph.svg

unused:
	cargo machete
