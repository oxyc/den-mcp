# den-mcp

Den's [MCP](https://modelcontextprotocol.io) server. Connect Claude or ChatGPT to Den Web's public origin and ask it
things like *"a male actor, 30–50, in 2020s films"*, *"movies like Heat but Scandinavian"*, *"slow-burn 90s
thrillers"* or *"who directed X, and what else did they make"*. It asks [den-atlas](https://github.com/oxyc/den-atlas)
and answers with what a model may see, and a link to the title's page in Den Web.

**Discovery only.** Nothing here reads or writes a library, and no library key comes near it.

```
Claude / ChatGPT ──OAuth 2.1──► den-edge (authorization server, consent in Den Web)
        │                           │ signs short-lived EdDSA access tokens
        └──POST /mcp (Bearer)──► den-edge relay ──► den-mcp ──► den-atlas (LAN)
                                                   verifies the token with den-edge's public key
```

## Tools

Titles are always `{type: movie|series, id}`; every title carries its Den Web `url`.

| Tool | Parameters | Asks atlas |
|---|---|---|
| `den_search` | `query`, `type?`, `year_min?`, `year_max?`, `language?`, `runtime_max?`, `page?`, `limit?` (10, ≤50) | `/index/query.json` |
| `den_filter_titles` | `type?` (all), `sel?` (`["kind:id", "-kind:id"]`), `page?`, `limit?` (20, ≤100) | `/index/filter/<type>/titles.json` |
| `den_filter_values` | `kind?`, `q?`, `type?`, `sel?`, `limit?` (10, ≤30) | `values/<kind>.json`; a person trait from `people/counts.json`; no kind: `counts.json` |
| `den_find_people` | `type?`, `sel?`, `role?`, `gender?`, `age_min?`, `age_max?`, `born_min?`, `born_max?`, `citizenship?`, `occupation?`, `page?`, `limit?` | `/index/filter/<type>/people.json` |
| `den_title` | `type`, `id` | `/index/title/<type>/<id>.json` |
| `den_similar` | `titles` (1–8), `sel?`, `mix_types?` (true), `page?`, `limit?` | `titles.json?sel=like:…` per title, interleaved |

Filter URLs are built in atlas's canonical spelling (`src/sel.rs`), held to atlas's own contract fixture
(`testdata/facets-canonical.json`), so the same question is one cache entry here and in atlas.

An age range becomes the birth decades it spans (atlas's `born` trait is one decade a question), one question each,
merged by credits and then held to the exact birth years; the answer says ages are ±1 and that people with no record
for a trait are never matched.

### What an answer may carry

Fields are **allowlisted** from atlas's answers, never blocklisted. Nothing from TMDB reaches a model — no overview,
poster, rating, vote count or popularity number, no streaming availability — because TMDB's terms forbid their use
in an AI application. Atlas's `/index/schema.json` names what of its answers is TMDB's (`tmdb.filterKinds`, read
hourly, with `rating` and `character` as a fixed floor): those kinds are refused as filters and never listed, and a
search hit named only by TMDB (`titleFrom: "tmdb"`) is left out and counted. A TMDB person id is used only to build a
Den Web person link, never returned. A last guard refuses any tool answer carrying prose under the keys atlas's own
serving guard refuses (`overview`, `plot`, `summary`, …).

Every list says it is drawn from Den's index (~47k titles), not the catalogue, and a title Den does not index is
`indexed: false`. Den's labels and plot facets come with their attribution: derived from Wikipedia article text
(CC BY-SA 4.0); people and credits are Wikidata's (CC0).

## Authorization

den-edge is the OAuth 2.1 authorization server (PKCE S256, dynamic client registration, RFC 8414 metadata). Only a
library member or the holder of a live guest invite can approve a connection. It signs access tokens as a compact JWS
(`alg: EdDSA`, `typ: at+jwt`): `iss`, `aud` (this server's `/mcp`), `sub` (an opaque session id, never a library or a
grant), `scope: den:search`, `exp`. They last 15 minutes and never past a guest's end of access.

This server checks each token with den-edge's public key alone — the algorithm is pinned, the signature strict — so
no request calls den-edge. den-edge's relay refuses a revoked session's tokens before they get here. A missing or
bad token is a 401 with `WWW-Authenticate: Bearer resource_metadata="<origin>/.well-known/oauth-protected-resource/mcp"`,
and that document (RFC 9728) names den-edge as the authorization server. Each session gets a token bucket
(`RATE_PER_MINUTE`, `RATE_BURST`).

## Routes

| Route | |
|---|---|
| `POST /mcp` | MCP Streamable HTTP: one JSON-RPC message a POST, answered as JSON; stateless, no `Mcp-Session-Id`. `initialize`, `ping`, `tools/list`, `tools/call`. `GET`/`DELETE` are 405 |
| `GET /.well-known/oauth-protected-resource[/mcp]` | RFC 9728 metadata |
| `GET /health` | `{"status":"ok"}`, or `degraded` with `no_token_keys` |
| `GET /metrics` | Prometheus text, with `Authorization: Bearer $METRICS_TOKEN`; a 404 otherwise |

One log line per request (`LOG_REQUESTS`): method, route, status, time, the tool's name and `rid=` — never its
arguments, which are what someone asked.

## Configuration

See `.env.example`. `ATLAS_URL` (plain http on the LAN) and `PUBLIC_ORIGIN` are required; `TOKEN_PUBLIC_KEYS` is
den-edge's public key (base64url, comma-separated during a rotation).

## Performance

Hyper and tokio directly, one runtime thread, no MCP SDK (the protocol surface is four methods), no TLS stack, and
a pooled keep-alive client to atlas with a 4 MB cache that honours atlas's `Cache-Control` and revalidates by ETag.
Measured with `cargo run --release --example load` (a stub atlas answering a 24-card page at once; macOS, M-series):

| | |
|---|---|
| startup, spawn to first `/health` answer | 4 ms |
| RSS idle / after 20k calls | 2.6 MB / 4.3 MB |
| a tool call's overhead on top of atlas, uncached | p50 0.15 ms, p99 0.39 ms |
| a tool call answered from the cache | p50 0.14 ms, p99 0.29 ms |
| 8 concurrent callers | ~8,000 calls/s, p50 0.97 ms, p99 1.4 ms |

The binary is ~1.4 MB.

## Development

```
cargo test
cargo run --example mint -- http://localhost:8096   # a dev key and a 15-minute token
ATLAS_URL=http://<atlas> PUBLIC_ORIGIN=http://localhost:8096 TOKEN_PUBLIC_KEYS=<key> cargo run
```

Releases: `scripts/release.sh X.Y.Z` — the fleet's script, which checks, bumps, tags, and waits for the signed image.
