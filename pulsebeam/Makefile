# Extract data using cargo metadata
# We filter for the package in the current directory to handle workspaces correctly
PKG_NAME    := $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].name')
PKG_VERSION := $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')

release:
	@if [ -z "$(TAG)" ]; then \
		echo "Error: TAG is required. Suggested: $(PKG_NAME)-v$(PKG_VERSION)"; \
		exit 1; \
	fi

	$(eval NEW_VERSION=$(shell echo $(TAG) | sed 's/$(PKG_NAME)-v//'))

	@echo "--- Step 1: Updating Cargo.toml via Cargo ---"
	sed -i '0,/^version = .*/s//version = "$(NEW_VERSION)"/' Cargo.toml

	@echo "--- Step 2: Generating Changelog ---"
	git-cliff \
		--include-path "pulsebeam-core/**" \
		--include-path "pulsebeam-proto/**" \
		--include-path "pulsebeam-runtime/**" \
		--include-path "pulsebeam/**" \
		--output CHANGELOG.md \
		--tag $(TAG)

	@echo "--- Step 3: Reviewing Diff ---"
	@git diff --color=always
	@echo ""
	@echo "Confirm release of $(PKG_NAME) v$(NEW_VERSION)? [y/N] " && read ans && [ $${ans:-N} = y ]

	@echo "--- Step 4: Finalizing Git ---"
	git add CHANGELOG.md Cargo.toml
	git commit -m "chore(release): $(PKG_NAME) $(NEW_VERSION)" -v
	git tag -a $(TAG) -m "Release $(TAG)"
