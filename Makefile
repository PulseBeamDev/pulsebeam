include makefiles/net.mk

SCCACHE := $(shell which sccache)
CARGO_CMD = RUSTC_WRAPPER=$(SCCACHE) cargo
TARGET_DIR = target/profiling
BINARY = $(TARGET_DIR)/pulsebeam

.PHONY: all help dev build release profile flamegraph perf deps brew-deps cargo-deps clean
all: build

dev:
	$(CARGO_CMD) run -p pulsebeam -- --dev

build:
	$(CARGO_CMD) build

release:
	$(CARGO_CMD) build --verbose --release -p pulsebeam

profile:
	$(CARGO_CMD) build --profile profiling -p pulsebeam

test: test-unit test-sim

test-unit:
	cargo test --workspace --exclude pulsebeam-simulator

test-sim:
	cargo test -p pulsebeam-simulator -- --no-capture

lint:
	cargo fix --allow-dirty && cargo clippy --fix --allow-dirty

flamegraph: profile
	taskset -c 2-5 $(CARGO_CMD) flamegraph --profile profiling -p pulsebeam --bin pulsebeam

perf: profile
	taskset -c 2-5 perf record --call-graph dwarf $(BINARY)
	hotspot perf.data

prepare-video:
	ffmpeg -r 30 -i video.h264 \
    -filter_complex "[0:v]split=3[v_f][v_h][v_q]; \
                     [v_h]scale=iw/2:-2[h_scaled]; \
                     [v_q]scale=iw/4:-2[q_scaled]" \
    -map "[v_f]" -c:v:0 libx264 -profile:v:0 baseline -b:v:0 1250k -maxrate:v:0 1250k -bufsize:v:0 2500k -g 60 -tune zerolatency -x264-params "annexb=1:repeat-headers=1" -bf 0 -r 30 full_f.h264 \
    -map "[h_scaled]" -c:v:1 libx264 -profile:v:1 baseline -b:v:1 400k -maxrate:v:1 400k -bufsize:v:1 800k -g 60 -tune zerolatency -x264-params "annexb=1:repeat-headers=1" -bf 0 -r 30 half_h.h264 \
    -map "[q_scaled]" -c:v:2 libx264 -profile:v:2 baseline -b:v:2 150k -maxrate:v:2 150k -bufsize:v:2 300k -g 60 -tune zerolatency -x264-params "annexb=1:repeat-headers=1" -bf 0 -r 30 quarter_q.h264

deps: deps-brew deps-cargo gh-deps

deps-brew:
	brew install git-cliff axodotdev/tap/cargo-dist

deps-cargo:
	$(CARGO_CMD) install cargo-release cargo-dist git-cliff
	$(CARGO_CMD) install flamegraph cargo-machete

deps-gh:
	gh extension install yusukebe/gh-markdown-preview

preview-markdown:
	gh markdown-preview

clean:
	$(CARGO_CMD) clean
	rm -f perf.data flamegraph.svg

chore-release:
	cargo release -v --execute

unused:
	cargo machete
