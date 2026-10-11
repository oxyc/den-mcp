# den-mcp

Den's [MCP](https://modelcontextprotocol.io) server. Connect Claude or ChatGPT to Den Web's public origin and ask it
things like *"a male actor, 30–50, in 2020s films"*, *"movies like Heat but Scandinavian"*, *"slow-burn 90s
thrillers"* or *"who directed X, and what else did they make"*. It asks [den-atlas](https://github.com/oxyc/den-atlas)
and answers with what a model may see, and a link to the title's page in Den Web.

**Discovery, queued changes and shared lists.** No library key comes near this server. With `MCP_WRITE_KEY`, a
connection the person allowed to make changes can queue them (below): den-mcp only *seals* a request to the library; a
TV or Den Web opens, checks and applies it. A connection the person allowed to read can see the watchlist, Continue
Watching and Seen history as a TV or Den Web last published them, sealed under a read key only the household and this
server hold.

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
| `den_find_people` | `type?`, `sel?`, `sort?` (prominence, credits, name, youngest, oldest), `role?`, `gender?`, `age_min?`, `age_max?`, `born_min?`, `born_max?`, `living?`, `citizenship?` (names, codes or Q-ids), `occupation?`, `birthplace?` (Q-ids), `birthcountry?` (names, codes or Q-ids; the country of the birthplace, sent as the code or item), `page?`, `limit?` | `/index/filter/<type>/people.json` with `order=`; an atlas that answers no `order` ordered by credits, and the answer's note says so |
| `den_title` | `type`, `id` | `/index/title/<type>/<id>.json` (with the ceremonies it won or was nominated at, `awards`) and `/index/studios/<type>/<id>.json` |
| `den_similar` | `titles` (1–8), `sel?`, `mix_types?` (true), `page?`, `limit?` | `titles.json?sel=like:…` per title, interleaved |
| `search` | `query` | `den_search`, as ChatGPT's research tools call it: `{results: [{id: "movie:949", title, url}]}` |
| `fetch` | `id` | `den_title` as text: `{id, title, text, url, metadata}` |

Every tool declares an `outputSchema` and answers with `structuredContent` and the same JSON as text; the tools above
are `readOnlyHint` and `idempotentHint`.

### Library writes

Listed only when `MCP_WRITE_KEY` is set **and** the token carries the `den:library.write` scope with a `dw` claim this
server opens (den-spec `wire/assistant-v1.md`). Title arguments are `type` (`movie` or `series`; sent as the library's
`tv`) and `id`, as the discovery tools return them. Each answers `{queued: true, id, note}`: nothing has happened yet.

| Tool | Parameters | Op |
|---|---|---|
| `den_watchlist_add` | `type`, `id` | `watchlist_add` |
| `den_watchlist_remove` | `type`, `id` (`destructiveHint`) | `watchlist_remove` |
| `den_mark_seen` | `type`, `id`, `value?` (true), `season?`, `episode?` (series; episode needs season) | `seen` |
| `den_rate` | `type`, `id`, `value` (`dislike`, `like`, `love` or null to clear) | `rate` |
| `den_play_on_tv` | `type`, `id`, `season?`, `episode?` (series; episode needs season) | `play` (TV queue) |
| `den_play_trailer` | `type`, `id` | `play_trailer` (TV queue) |
| `den_tv_status` | — | `GET /assistant/tv` (no request) |

Den's UI calls `dislike` **Not for me**, and it is also a taste signal that steers recommendations away: the tool's
description tells a model to use it for "not for me", "not interested" and "stop suggesting this".

A call opens the grant key from `dw` (`MCP_WRITE_KEY`, bound to the token's `sub`), asks den-edge
`GET /assistant/dropbox` for the library's drop-box key (kept a minute per session), signs and seals the request with
`den-assistant` and `POST /assistant/append`s it, all with the caller's own token. The grant key lives for that call.
den-edge's `queue_full`, rate limits and 401/403 are said to the model in plain words. The log line for a write is the
tool and a status word, never an argument or key.

**Playing on the TV** (den-spec §16). `den_play_on_tv` and `den_play_trailer` are requests like the others, sealed
and signed the same way, but they belong in the TV queue (`den_assistant::Queue::of`): the append carries
`"queue": "tv"`, which only a TV with Den open drains, and a play older than two minutes is stale. So before sealing
anything they ask `GET /assistant/tv`; if the TV is not online the answer is "Open Den on the TV first" and nothing is
queued. After the append they ask `GET /assistant/request/{seq}?queue=tv` every 0.4 s for up to 8 s and answer
`state: "taken"` ("The TV picked it up") or `"queued"` (not yet). `taken` is all den-edge can know: whether anything
played is not observable, so the note says that a connection made before TV playback has no play record, its plays are
refused at the TV, and the owner reconnects the assistant with "Allow changes to my library and playing on my TV"
ticked. `den_tv_status` answers `{online, last_seen_at}`. These tools are listed with the write tools (a token does not
say whether its connection may play).

`cargo run --example writekey` makes a key pair: `MCP_WRITE_KEY` for den-mcp, `MCP_WRITE_PUBLIC_KEY` for den-edge.

### Library reads

Listed only when `MCP_WRITE_KEY` is set **and** the token carries the `den:library.read` scope with a `dr` claim this
server opens for the token's session (den-spec `wire/assistant-v1.md` §15); a read-only connection needs no write
scope. Every one is `readOnlyHint` and paged (`page?`, `limit?` 20, ≤100), with `total`, `more`, `as_of` (when a device
published the lists) and a `note` that says the lists are only as fresh as the last time a TV or Den Web was open. The
projection is the whole library, never cut short, so `limit` is only the page size; `more` says there is another page. Titles are named through atlas like every discovery tool (name, year, genre,
Den Web `url`; no TMDB field), with their ids and dates (UTC, RFC 3339) kept.

| Tool | Parameters | From the projection |
|---|---|---|
| `den_watchlist` | `type?`, `page?`, `limit?` | `watchlist`, newest added first |
| `den_continue_watching` | `page?`, `limit?` | `continue`: `action` `resume` (with `fraction`) or `next`, `season`, `episode` |
| `den_history` | `since?`, `until?` (UTC date or time), `type?`, `page?`, `limit?` | `seen`, newest first; entries with no date only without a time range |
| `den_ratings` | `value?` (love, like, dislike or all; `dislike` is Not for me), `type?`, `page?`, `limit?` | `rated`, newest first; each has `reaction`, and `not_for_me` for a dislike |
| `den_recommend` | `type?` (movie, series, all), `include_watchlist?` (false), `page?`, `limit?` (10, ≤100) | all of it, as the body of atlas's `POST /recommend` (below) |

`den_recommend` ("what should we watch") asks atlas to rank, as Den Web's billboard does, so nothing here ranks. The body
is Den Web's: `library` is every title watched or in progress (weight 1), on the watchlist (0.6) or rated, with the time
it was last touched and Den Web's weights (`weightOf`: a like adds 0.5, a love 1, and a dislike, "Not for me", is −1.5
outright), the recent 180 days if there are any (a library title with no date says nothing about now), at most 5,000
and 500 KiB; `owned` is every title atlas must not show (all watched, in progress or rated, and the watchlist unless
`include_watchlist`), at most 10,000, so a title marked Not for me is never shown; `hide` is the household's own rules
(`minYear`, `genres`, `languages`, `anime`) from the projection, sent as Den Web sends them; `surface` is `home`,
`movies` or `series`; `limit` the page and one more. There are no `candidates`. A title is named like every other
result, with its Den Web `url`; `why` is the scorer's reason code in Den's own words (`similar`, `profile`, `people`,
`franchise`, `arrived`, `recent`, `upcoming`, `timely`), never its numeric terms, and `quality` and `buzz` give none
since they weigh ratings and popularity. Atlas ranks at most 300, so `more` ends there; `include_watchlist` marks
watchlisted results `on_watchlist`. One request is built and asked at a time, so it costs the memory of the list tools.

The projection reader ignores what it does not know (den-spec §15 requires it): unknown members of an object, and
elements past the ones a row is read for. A projection published before `rated` and `hide` reads with none.

A call opens the read key from `dr`, checks it names the same grant as `dw` when the token has both, `GET`s
`/assistant/projection` with the caller's own token (`{library, grant, set, parts}`), opens the sealed, compressed parts
with `open_projection_compact` (which checks the set whole and in order, its library and grant) and answers from it. A
`404 no_projection` says to open Den on the TV or web once. The log line is the tool and a status word.

The container has 32 MB, so a history of tens of thousands of plays is never built as JSON values: the opened compact
form is read once into packed fixed-size rows (`src/projection.rs`, about 1 MB for 5,000 watchlist entries, 500 in
Continue Watching and 50,000 plays), the page is picked out of those in one pass, and only that page is named through
atlas. A session's calls share one open (an assistant often calls all three tools together) and the rows are kept for
30 s, at most 4 MB across sessions, then wiped, so a list is as stale as that on top of the last publish. Measured with
`cargo build --release --examples && target/release/examples/readload <real|large> <concurrent> [rounds]` (peak resident
set, idle 3.3 MB):

| | 1 call | 3 at once | 8 at once |
|---|---|---|---|
| real size, 62 KB sealed | 6.0 MB | 6.5 MB | 7.1 MB |
| large, 535 KB sealed (5,000 / 500 / 50,000) | 11.3 MB | 11.8 MB | 12.4 MB |
| `den_recommend` only, real size | 5.8 MB | 6.1 MB | 6.4 MB |
| `den_recommend` only, large | 11.1 MB | 11.6 MB | 11.5 MB |

### Open-vocabulary facets (scaffold only)

`src/open_facets.rs` contains the no-network execution boundary for
[open-vocabulary facets](https://github.com/oxyc/den-mcp/issues/6): versioned normalized-question/schema digests,
corpus/model-scoped run keys, atomic decision persistence and resume, conservative pre-call ceilings for calls,
input tokens and cost, and coverage with an explicit denominator, unknown count and incomplete status. Its tests use
a deterministic fake provider and assert that source evidence is never persisted.

No MCP tool or paid provider is wired to that module yet. In particular, this scaffold cannot make Jev or Haiku
calls. Atlas preselection, the production adapter, opt-in configuration and the queued permanent-facet path remain
tracked in issue #6.

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

Awards are title kinds by ceremony, as atlas holds them (Wikidata's P166/P1411, no categories): `award:<Q>` won or
nominated there, `won:<Q>` won there, the ceremonies from `den_filter_values kind=award`. `author:<Q>` is a title
adapted from that author's work. An atlas without these sections answers without them and names them in `ignored`.

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
`indexed: false`. Since den-atlas 0.93.0 atlas names such a title from Wikidata when it can (`source: "wikidata"`): the
tools that name a title (the read tools, `den_recommend`, `den_title`, `fetch`) then carry `title`, `year` and
`original_title` — only those, allowlisted — with `name_source: "wikidata"` and a note crediting Wikidata (CC0); a title
Wikidata does not name, or whose name has another source, stays without one. Den's labels and plot facets come with
their attribution: derived from Wikipedia article text (CC BY-SA 4.0); people and credits are Wikidata's (CC0).

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
den-edge's public key (base64url, comma-separated during a rotation). `MCP_WRITE_KEY` (optional, base64url of 32
bytes) turns on the write tools and then needs `EDGE_URL`, den-edge's bare `http://` origin on den.network.

`den_open_facet` is non-billing by default: it returns an explicit `den_search` fallback without contacting Atlas
or a provider. A deployment must set `OPEN_FACETS_ENABLED=1` and every spend bound in `.env.example`, a pinned
provider model, a dataset-specific corpus id, a writable decision directory, and `TYPESAFE_API_KEY` before the paid
path exists. The tool embedding-preselects 50 Atlas cards by default and at most 100, sends Jev only the
MCP-allowlisted card plus Den's own labels (subgenres, moods, plot facets, as `den_title` serves them), reserves
calls/tokens/cost before each request, persists only fixed yes/no/unknown decisions,
and reports both preselection and classification denominators. Provider failure returns an incomplete result and the free
fallback. Calls serialize at this boundary so simultaneous identical requests cannot double-spend before saving.
Each normalized question is counted in the decision directory; `GET /open-facets/queue` (metrics token) lists those
asked three or more times, the candidates for a corpus-wide permanent facet.

## Performance

Hyper and tokio directly, one runtime thread, no MCP SDK (the protocol surface is four methods), and a pooled
keep-alive client to atlas with a 4 MB cache that honours atlas's `Cache-Control` and revalidates by ETag. The
binary includes rustls solely for the disabled-by-default open-facet provider path.
Measured with `cargo run --release --example load` (a stub atlas answering a 24-card page at once; macOS, M-series):

| | |
|---|---|
| startup, spawn to first `/health` answer | 4 ms |
| RSS idle / after 20k calls | 2.6 MB / 4.3 MB |
| a tool call's overhead on top of atlas, uncached | p50 0.15 ms, p99 0.39 ms |
| a tool call answered from the cache | p50 0.14 ms, p99 0.29 ms |
| 8 concurrent callers | ~8,000 calls/s, p50 0.97 ms, p99 1.4 ms |

The release binary is ~3.4 MB (2.85 MB before library writes and reads; `den-assistant`'s X-Wing, HPKE, AES-GCM and DEFLATE add ~0.38 MB).

## Development

```
cargo test
cargo run --example mint -- http://localhost:8096   # a dev key and a 15-minute token
ATLAS_URL=http://<atlas> PUBLIC_ORIGIN=http://localhost:8096 TOKEN_PUBLIC_KEYS=<key> cargo run
```

Releases: `scripts/release.sh X.Y.Z`. It changes only `VERSION`, pushes that commit, waits for its exact main CI, then
creates `vX.Y.Z` and waits for the signed image. Do not bump `Cargo.toml`: the image supplies `DEN_VERSION` at run
time (startup line, `initialize`'s `serverInfo`, `mcp_build_info`), so a release reuses the cached Rust build.

Main CI is the release gate and artifact producer: it tests, builds, probes (`scripts/probe-image.sh` starts the image
and reads its version from `/metrics`), scans, and records an immutable `candidate-<sha>` digest. A tag only verifies
and promotes that digest (sign, version tags, `:latest`) in `docker-publish`; it never rebuilds. A failed main run
creates no tag and cannot move `:latest`. The Monday/manual run still rebuilds the newest tag with no cache, scans it
(OS packages gate), signs it and moves `:latest`.
