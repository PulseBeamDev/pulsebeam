# Extract package name for the commit and tag prefix
PKG_NAME := $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].name')

release:
	@if [ -z "$(V)" ]; then \
		echo "Error: V is required. Example: make release V=0.3.2"; \
		exit 1; \
	fi

	@# Construct the full tag: e.g., pulsebeam-v0.3.2
	$(eval FULL_TAG=$(PKG_NAME)-v$(V))

	@echo "--- Step 1: Updating Cargo.toml to $(V) ---"
	sed -i '0,/^version = .*/s//version = "$(V)"/' Cargo.toml
	
	@echo "--- Step 2: Generating Changelog for $(FULL_TAG) ---"
	git-cliff \
		--include-path "pulsebeam-core/**" \
		--include-path "pulsebeam-proto/**" \
		--include-path "pulsebeam-runtime/**" \
		--include-path "pulsebeam/**" \
		--output CHANGELOG.md \
		--tag $(FULL_TAG)

	@echo "--- Step 3: Reviewing Diff ---"
	@git diff --color=always CHANGELOG.md Cargo.toml
	@echo ""
	@echo "Confirm release of $(PKG_NAME) version $(V) (Tag: $(FULL_TAG))? [y/N] " && read ans && [ $${ans:-N} = y ]

	@echo "--- Step 4: Finalizing Git ---"
	git add CHANGELOG.md Cargo.toml
	git commit -m "chore(release): $(PKG_NAME) $(V)" -v
	git tag -a $(FULL_TAG) -m "Release $(FULL_TAG)"
	
	@echo "Successfully tagged $(FULL_TAG)!"
