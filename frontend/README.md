# kigicli.dev

The Kigi landing site and the Worker that fronts it, deployed to Cloudflare as
the `kigi-frontend` Worker on `kigicli.dev` and `www.kigicli.dev`.

## Layout

| Path            | What it is                                                        |
| --------------- | ----------------------------------------------------------------- |
| `worker/index.ts` | Worker entry point: installer proxy, `/health`, asset fallthrough |
| `wrangler.jsonc`  | Deployment config (custom domains, assets binding, SPA fallback)  |
| `dist/`           | The built site served by the assets binding                       |

> **`dist/` is build output, not source.** It was recovered by mirroring the
> live `kigicli.dev` deployment — the original Vite/React project that produced
> these bundles is not in this repo. Content and copy changes therefore have to
> be made against the hashed `dist/assets/index-*.js` and `index-*.css` files
> directly. If the upstream source ever turns up, drop it in beside `dist/` and
> switch `wrangler.jsonc` to point at its build directory.

## Routes

Assets are matched first and served directly. Requests with no matching file
fall through to the Worker, which owns:

- `GET /install.sh` — proxies `install.sh` from the Kigi repo on Gitea
- `GET /install.ps1` — proxies `install.ps1` from the Kigi repo on Gitea
- `GET /health` — plain-text liveness check
- everything else — `env.ASSETS.fetch()`, which serves `index.html` for unknown
  paths (single-page-application fallback)

The installer routes are proxied rather than linked so the published install
command (`curl -fsSL https://kigicli.dev/install.sh | bash`) stays stable no
matter where the repo is hosted. Responses are cached for 10 minutes at the
edge, so a push to `main` takes up to that long to show up.

Because assets win over the Worker, never add `install.sh` or `install.ps1` to
`dist/` — they would shadow the proxy routes.

## Deploy

```sh
npm install
npx wrangler deploy
```

Cloudflare Web Analytics injects its beacon `<script>` at the edge; that is why
`dist/index.html` does not contain one and must not have one added.
