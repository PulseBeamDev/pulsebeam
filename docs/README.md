# PulseBeam documentation

The official PulseBeam documentation site, built with VitePress.

## Preview locally

Use a Linux x86_64 checkout with the [host prerequisites](build.md#host-prerequisites)
installed. Run commands from the repository root; Bazel provisions Node and the
site dependencies. Initial downloads require network access.

```bash
./bazel run //:docs_dev -- --host 127.0.0.1
```

Open the local URL printed by VitePress. The documentation home page should
appear, and edits to site pages should reload in the browser. Stop the development
server with Ctrl+C.

## Validate changes

```bash
./bazel test //docs:check
```

This builds the static site and checks its internal links. `README.md` and
`AGENTS.md` are excluded from site pages, so check their links separately.

The [React quickstart](react/quickstart.md) includes its actual `Room.tsx` and
`main.tsx` sources. `//agents/react:quickstart_types` checks them against the built
public packages, and `//agents/pulsebeam-agent-web:browser` exercises the same
bundled application with two participants and real browser media.
## Publish the documentation

The site is served at <https://docs.pulsebeam.dev> by Cloudflare Workers Static
Assets, not Cloudflare Pages or GitHub Pages. Bazel builds the site; Wrangler
uploads the resulting files without rebuilding them.

From the repository root, validate and inspect the deployment bundle:

```sh
./bazel test //docs:check
npx --yes wrangler@4.149.0 deploy --config docs/wrangler.jsonc --dry-run
```

For an authorized publication, authenticate to the Cloudflare account that owns
the active `pulsebeam.dev` zone, then deploy:

```sh
npx --yes wrangler@4.149.0 login
npx --yes wrangler@4.149.0 deploy --config docs/wrangler.jsonc
```

For noninteractive deployment, set `CLOUDFLARE_API_TOKEN` and
`CLOUDFLARE_ACCOUNT_ID` instead of logging in. Scope the token to the intended
account and zone using Cloudflare's **Edit Cloudflare Workers** template. Keep
credentials outside the repository.

The configuration binds `docs.pulsebeam.dev` as a Workers Custom Domain.
Cloudflare manages its DNS record and TLS certificate. During the first
migration, Wrangler replaces any conflicting DNS record for this hostname. In an
interactive terminal it asks for confirmation; noninteractive deployment replaces
conflicting records without prompting. Inspect the existing record and authorize
that one-time cutover with the zone owner before deploying; do not modify other
hostnames.

After deployment, open the homepage and `/react/quickstart` directly. A nonexistent
path must return HTTP 404, not the homepage. Clean URLs and VitePress's generated
`404.html` are handled by Workers Static Assets. The `workers.dev` hostname and
version preview URLs are disabled.
