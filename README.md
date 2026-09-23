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
| `den_search` | `query`, `type?`, `year_min?`, `year_max?`, `language?`, `runtime_max?`, `broadcaster?`, `page?`, `limit?` (10, ≤100) | `/index/query.json`; its count is `candidates`, how many titles the words reached |
| `den_filter_titles` | `type?` (all), `sel?` (`["kind:id", "-kind:id"]`), `order?` (popular; newest/oldest are noted as unavailable), `page?`, `limit?` (20, ≤100) | `/index/filter/<type>/titles.json`; each applied kind's coverage as `on_record` |
| `den_filter_values` | `kind?`, `q?`, `kinds?`, `type?`, `sel?`, `limit?` (10, ≤10) | a short-list kind whole from `counts.json`; others `values/<kind>.json`; a person trait `people/values/<trait>.json` (den-atlas#80), else `people/counts.json`; no kind: `counts.json` |
| `den_find_people` | `type?`, `sel?`, `sort?` (prominence, credits, name, youngest, oldest), `role?`, `gender?`, `age_min?`, `age_max?`, `born_min?`, `born_max?`, `living?`, `citizenship?` (names, codes or Q-ids), `occupation?`, `page?`, `limit?` | `/index/filter/<type>/people.json` with `order=`; an atlas that answers no `order` ordered by credits, and the answer's note says so |
| `den_title` | `type`, `id` | `/index/title/<type>/<id>.json` and `/index/studios/<type>/<id>.json` |
| `den_similar` | `titles` (1–8), `sel?`, `mix_types?` (true), `page?`, `limit?` | `titles.json?sel=like:…` per title, interleaved |
| `search` | `query` | `den_search`, as ChatGPT's research tools call it: `{results: [{id: "movie:949", title, url}]}` |
| `fetch` | `id` | `den_title` as text: `{id, title, text, url, metadata}` |

Every tool declares an `outputSchema` and answers with `structuredContent` and the same JSON as text; all are
`readOnlyHint` and `idempotentHint`.

Filter URLs are built in atlas's canonical spelling (`src/sel.rs`), held to atlas's own contract fixture
(`testdata/facets-canonical.json`), so the same question is one cache entry here and in atlas.

Either of several values of one kind is one item (den-atlas#89): `country:FR|IT` in `sel`, and a list in a trait
argument (`citizenship: ["American", "British"]`, `role: ["cast", "director"]`); a list of lists holds every group
(`[["US"], ["GB"]]`: dual citizens). Separate items still all hold. A group is sent sorted, each value once, with a
literal `|`, and counts toward the 16-value cap by its values. A value atlas does not recognise is named in the
answer's notes; the rest of its group still applies.

A value is read the way a person writes it. Languages and countries may be named (`language:Swedish`,
`country:Sweden`, a citizenship "Swedish" or "SE" → Q34), from Wikidata's tables in `src/names_table.rs`
(`scripts/names-table.py` writes it). A short-list kind (moods, subgenres, regions, plot facets, …) is read against
Den's own vocabulary — `counts.json` with no selection, kept for the hour atlas marks it fresh — whatever the case or
separators (`mood:tense edge of seat` is `mood:Tense/Edge-of-seat`), and the answer's `read_as` says so; a value Den
does not have is refused with the nearest three.

An age range becomes one birth-year range, atlas's `born:<from>-<to>` trait (den-atlas#85), with an open end left open
(`born:1976-`, `born:-1996`), and pages like any other list. An atlas older than ranges answers that trait with a 400;
den-mcp then asks the decades the range spans, one question each, merged and held to the exact birth years, within
the first 100 people, and the answer's note says so. The answer says ages are ±1 and that people with no record for a
trait are never matched.

### What an answer may carry

Fields are **allowlisted** from atlas's answers, never blocklisted. Nothing from TMDB reaches a model — no overview,
poster, rating, vote count or popularity number, no streaming availability — because TMDB's terms forbid their use
in an AI application. Atlas's `/index/schema.json` names what of its answers is TMDB's (`tmdb.filterKinds`, read
hourly, with `rating` and `character` as a fixed floor): those kinds are refused as filters and never listed, and a
search hit named only by TMDB (`titleFrom: "tmdb"`) is left out and counted. A person's TMDB id appears only inside
their Den Web link (`/person/<id>-<name>`), which is how Den Web addresses people — an identifier, never a field of
its own. A last guard refuses any tool answer carrying prose under the keys atlas's own serving guard refuses
(`overview`, `plot`, `summary`, …).

Every list says it is drawn from Den's index (~47k titles), not the catalogue, and a title Den does not index is
`indexed: false`. Den's labels and plot facets come with their attribution: derived from Wikipedia article text
(CC BY-SA 4.0); people and credits are Wikidata's (CC0).

## Authorization

den-edge is the OAuth 2.1 authorization server (PKCE S256, dynamic client registration, RFC 8414 metadata). Only a
library member or the holder of a live guest invite can approve a connection. It signs access tokens as a compact JWS
(`alg: EdDSA`, `typ: at+jwt`): `iss`, `aud` (this server's `/mcp`), `sub` (an opaque session id, never a library or a
grant), `scope: den:search`, `exp`. They last 15 minutes and never past a guest's end of access.

This server checks each token with den-edge's public key alone — the algorithm is pinned, the type must be `at+jwt`,
the signature strict — so no request calls den-edge. den-edge's relay refuses a revoked session's tokens before they get here. A missing or
bad token is a 401 with `WWW-Authenticate: Bearer resource_metadata="<origin>/.well-known/oauth-protected-resource/mcp"`,
and that document (RFC 9728) names den-edge as the authorization server. Each session gets a token bucket
(`RATE_PER_MINUTE`, `RATE_BURST`).

## Routes

| Route | |
|---|---|
| `POST /mcp` | MCP Streamable HTTP: one JSON-RPC message a POST (a batch of up to 16 under 2025-03-26, the version a request naming none is taken to speak), answered as JSON; stateless, no `Mcp-Session-Id`. `initialize`, `ping`, `tools/list`, `tools/call`. `GET`/`DELETE` are 405. At most 32 tool calls run at once (past that a call is told Den is busy) and 16 requests go to atlas at once |
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
