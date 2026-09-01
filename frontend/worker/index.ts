/**
 * kigicli.dev — static site + installer proxy.
 *
 * Static assets are served directly by the assets binding (SPA fallback). Only
 * paths that do not match an asset reach this Worker, which is where the two
 * installer routes live:
 *
 *   GET https://kigicli.dev/install.sh   → install.sh  from the Kigi repo
 *   GET https://kigicli.dev/install.ps1  → install.ps1 from the Kigi repo
 *
 * Keeping these behind our own domain means the published install command never
 * has to change when the source host does.
 */

interface Env {
  ASSETS: Fetcher;
}

/** Raw-file base for the canonical Kigi repo (GitHub, `main` branch). */
const UPSTREAM = "https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main";

/** Human-facing repo URL, used in the installer-unavailable message. */
const REPO_URL = "https://github.com/ZacharyZhang-NY/Kigi-CLI";

interface Installer {
  file: string;
  contentType: string;
  /** Emitted when upstream is unreachable — must be valid in the target shell. */
  unavailable: (status: number) => string;
}

const INSTALLERS: Record<string, Installer> = {
  "/install.sh": {
    file: "install.sh",
    contentType: "text/x-shellscript; charset=utf-8",
    unavailable: (status) =>
      `echo "kigicli.dev: installer unavailable (upstream ${status}). Try ${REPO_URL}" >&2; exit 1\n`,
  },
  "/install.ps1": {
    file: "install.ps1",
    contentType: "text/plain; charset=utf-8",
    unavailable: (status) =>
      `Write-Error "kigicli.dev: installer unavailable (upstream ${status}). Try ${REPO_URL}"; exit 1\n`,
  },
};

async function serveInstaller(
  request: Request,
  path: string,
  ctx: ExecutionContext,
): Promise<Response> {
  const { file, contentType, unavailable } = INSTALLERS[path];
  const cache = caches.default;
  const cacheKey = new Request(`${UPSTREAM}/${file}`, { method: "GET" });

  let upstream = await cache.match(cacheKey);
  if (!upstream) {
    upstream = await fetch(cacheKey, {
      headers: { "User-Agent": "kigicli.dev-installer-proxy" },
      cf: { cacheTtl: 600, cacheEverything: true },
    });
    if (upstream.ok) {
      const cacheable = new Response(upstream.clone().body, upstream);
      cacheable.headers.set("Cache-Control", "public, max-age=600");
      ctx.waitUntil(cache.put(cacheKey, cacheable));
    }
  }

  if (!upstream.ok) {
    return new Response(unavailable(upstream.status), {
      status: 502,
      headers: { "Content-Type": contentType },
    });
  }

  const headers = new Headers({
    "Content-Type": contentType,
    "Cache-Control": "public, max-age=600",
    "X-Content-Type-Options": "nosniff",
  });

  console.log(
    JSON.stringify({
      event: "install_script",
      script: path,
      ua: request.headers.get("User-Agent") ?? "",
      ref: request.headers.get("Referer") ?? "",
    }),
  );

  return new Response(upstream.body, { status: 200, headers });
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname in INSTALLERS) {
      if (request.method !== "GET" && request.method !== "HEAD") {
        return new Response("Method Not Allowed", {
          status: 405,
          headers: { Allow: "GET, HEAD" },
        });
      }
      return serveInstaller(request, url.pathname, ctx);
    }

    if (url.pathname === "/health") {
      return new Response("ok\n", { headers: { "Content-Type": "text/plain" } });
    }

    return env.ASSETS.fetch(request);
  },
} satisfies ExportedHandler<Env>;
