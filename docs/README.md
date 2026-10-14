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

The [React quickstart](react/quickstart.md) uses short, framework-independent
SDK snippets. The separate Vite browser fixture in `react/Room.tsx` and
`react/main.tsx` is still exercised by `//agents/react:quickstart_types` and
`//agents/pulsebeam-agent-web:browser`; those tests don't cover every React
framework.

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
Cloudflare manages its DNS record and TLS certificate. Before the first
publication, inspect the existing DNS record and authorize the one-time cutover
with the zone owner. Wrangler attempts to replace conflicting records, but
Cloudflare can reject externally managed records with error `100117`. In that
case, remove only the approved old `docs.pulsebeam.dev` record through the
Cloudflare dashboard or a zone-scoped **DNS Edit** API token, then rerun the deploy
command. Wrangler's login credential does not necessarily include DNS access.
Do not modify other hostnames.

After deployment, open the homepage and `/react/quickstart` directly. A nonexistent
path must return HTTP 404, not the homepage. Clean URLs and VitePress's generated
`404.html` are handled by Workers Static Assets. The `workers.dev` hostname and
version preview URLs are disabled.
