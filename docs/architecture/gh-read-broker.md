# Session `gh` read broker (#1743)

One agent session with a few subagents can spend GitHub's shared 5,000/hour
REST budget on its own: watchers, `gh api` polls and status checks re-fetch
the same objects over and over, and then every tool on the machine fails at
once. The read broker answers repeated in-session `gh api` GETs from a
durable cache in the clud daemon. Within a short TTL a repeat costs no
request; after it, the broker revalidates with the stored ETag, and GitHub
does not charge an authorized `304 Not Modified` against the rate limit.

The broker answers **only `gh api <endpoint>` GETs**. Phase 1 is a
read-through ETag cache keyed by the exact URL. Phase 2 adds
[merged collections](#merged-collections): comment and run-list reads share
one stored membership across page sizes and are rebuilt in the shape the
caller's query returns, and [targeted invalidation](#write-invalidation).
Every refresh of a merged collection re-sends its upstream pages with their
ETags, so a deletion, a reaction or a rerun is seen on the next refresh at no
rate-limit cost ([DD-153](../DESIGN_DECISIONS.md#dd-153-merged-gh-reads-revalidate-every-page-instead-of-querying-a-delta)).
Phase 3 adds [subscriptions](#subscriptions-phase-3), a daemon route a
waiter blocks on until one of its reads changes, and the
[rate-limit floor](#rate-limit-floor-phase-3), which defers background
refreshes when the budget runs low
([DD-154](../DESIGN_DECISIONS.md#dd-154-gh-waiters-block-on-broker-subscriptions-and-background-refreshes-yield-below-a-reserve)).
Porcelain commands (`gh pr view`, `gh run view`, ...) are not brokered.

## Path of one read

```
agent: gh api repos/o/r/actions/runs/7 --jq .status
  │
  ▼  session gh alias (shim_main.rs gh_shim::relay)
  │   gh_broker::classify::api_read   → a brokerable GET?   no → real gh
  │   gh_broker::client::read         → POST /gh/read        miss → real gh
  │      (daemon.json: dashboard_port + capability cookie)
  ▼
daemon HTTP thread (daemon/http.rs → spawn_gh_read)
  │   gh_broker::service::GhBroker::read
  │     fresh within TTL? ─────────────────────────► cached body ("cache")
  │     single-flight: one leader per key, others wait ("cache")
  │     leader: real `gh api -i <endpoint> [-H If-None-Match: <etag>]`
  │        304 → cached body, TTL restarts ("304")
  │        200 → store body + ETag ("full")
  │        non-2xx → 409 to the shim ("passthrough")
  ▼
shim: gh_broker::client::serve_replay(body) on 127.0.0.1:<port>/clud-gh-replay/<random>
  │   real gh api http://127.0.0.1:<port>/clud-gh-replay/<random> --jq .status
  ▼
caller's stdout: what the real gh prints for that body
```

## Output contract

The shim never formats output itself. After the broker answers, it reruns
the caller's own `gh api` argv on the real `gh`, with only the endpoint word
replaced by a one-shot loopback URL that serves the brokered body with the
upstream headers (`gh` accepts an absolute URL as the endpoint). `--jq`,
`--template`, `--silent`, TTY pretty-printing and colors are therefore `gh`'s
own code over the same body bytes, so the output is identical by
construction. Go never proxies loopback; the only credential `gh` could
attach for the loopback host (`GH_ENTERPRISE_TOKEN`) reaches the shim's own
listener. The replay path is a 128-bit random token; any other path
gets a 404.

What would not be identical is never brokered. `classify::api_read` accepts
only `--jq`/`-q`, `--template`/`-t`, `--silent`, `-X GET`/`--method GET` and
`--hostname`. It passes through everything else to the real `gh` unchanged:
`-i`/`--include` (the headers would name the loopback), `--paginate`,
`--slurp`, `-H`, `-p`, `--cache`, `--verbose`, any body flag
(`-f`/`-F`/`--field`/`--raw-field`/`--input`), `graphql`, absolute URLs,
`{owner}`-style placeholders (`gh` fills them from the checkout), non-UTF-8
argv, and any flag it does not know. Only `2xx` responses are replayed. A
non-2xx upstream status makes the shim run the real `gh`, so error text and
exit codes are `gh`'s own. That costs a second request for errors only.

## Fail open

Every shim-side miss runs the real `gh` with the original argv:

- the setting is off (`CLUD_GH_READ_BROKER` is set and not `1`, or it is
  absent and `git.gh_read_broker` is `false`; see [Setting](#setting));
- `daemon.json` is missing or names a dead port (500 ms connect timeout);
- an older daemon without the route answers 404;
- the broker answers non-200: upstream non-2xx (409), a store or transport
  error (502), or a rejected request (400);
- the 45 s request deadline passes.

The daemon is lazy and can retire when idle. A foreground session holds a
lease on it, so in practice it is up while a session runs, but the shim never
starts one.

## Identity and the upstream `gh`

The broker reaches GitHub only through the real `gh` (`upstream::GhCli`),
so authentication, enterprise hosts and proxies stay `gh`'s. The shim sends
its `CLUD_GH_SHIM_TARGET` and the caller's values of
`gh_broker::FORWARDED_ENV`: `GH_HOST`, the token variables, `GH_CONFIG_DIR`,
the config-home and proxy variables. The daemon runs that `gh` with its own environment, where each of
those keys is replaced by the caller's value or removed. It strips debug,
pager and forced-color variables and adds `GH_PROMPT_DISABLED=1` and
`NO_COLOR=1`, so the `-i` output it parses is plain. It first revalidates the
request: the path must be named `gh` and pass `shim_registry::valid_target`
(absolute, executable, not a clud alias, not a copy of clud), the endpoint and
hostname must pass the classifier's checks, and env keys must be in
`FORWARDED_ENV`. The cache key hashes
endpoint, `--hostname`, `gh` path and the forwarded values, so two identities
never share a body. Tokens are hashed into the key and never stored or
logged. The route is behind the dashboard capability cookie and Host check,
like `/telemetry/log`.

## Merged collections

`gh_broker::collection::plan` recognizes three collection endpoints. They are
keyed by path and membership filters, not by the exact URL, so `per_page=5`
and `per_page=100` share one membership:

| Endpoint | Upstream pages | Natural order |
| --- | --- | --- |
| `repos/{o}/{r}/issues/{n}/comments` | `?per_page=100`, every page (at most 10) | id ascending |
| `repos/{o}/{r}/pulls/{n}/comments` | same | id ascending |
| `repos/{o}/{r}/actions/runs` | the caller's filters `&per_page=<width>`, the newest page only | `created_at` descending, then id |

A run list's width is the narrowest of 10, 30 and 100 that covers the
caller's `per_page` (default 30). A wider caller re-fetches the list at its
width; the width never shrinks, so a narrower caller is cut from the wider
page. One page of 100 runs is about 1.2 MB on a busy repo, so a `per_page=5`
reader re-downloads 10 runs rather than 100 when the list changes.

**Refresh: every page, conditionally.** The broker stores each upstream page
as it last received it, with its ETag. After the TTL it re-sends every page
with `If-None-Match`:

- `304`: the page is byte-identical to the stored copy. GitHub does not count
  an authorized `304` against the rate limit; measured on 2026-10-02, 15
  conditional requests moved `X-RateLimit-Used` by the same background
  drift as 15 calls to the free `rate_limit` endpoint.
- `200`: the page is replaced. GitHub's ETag covers the whole body, so a
  deletion, an edit, a reaction count, a review comment going outdated and a
  rerun (`run_attempt`, `status`) all change it, whether or not they move
  `updated_at`.

A page ends the collection only when it holds fewer objects than the page
size. A full page that is a `304` is followed by the next page anyway: a new
comment can open page 2 without changing page 1. A collection of exactly 100
comments therefore keeps an empty page 2, which costs one free `304`.

**Pages that do not fit together.** If a pass replaced any page of a
multi-page collection, the broker re-checks every page before the last with
the ETag it just received. A deletion that lands between two page requests
shifts the later pages: without the re-check the stored copy of page 1 would
keep the deleted comment, and the comment that moved onto page 1 would be in
neither copy. Any re-check that is not a `304`, and any joined membership out
of natural order or with an id twice, runs the pass again from the pages just
received. If the second pass does not fit either, that read takes the
exact-URL path; the stored pages are kept, so the next refresh revalidates
them as usual. (A page the merge cannot parse, a short page that announces
another, or more than 10 pages instead mark the collection unmergeable for
30 minutes, below.)

**Cost.** A quiet collection costs one free `304` per page per TTL. A change
costs one charged `200` per changed page, plus free re-checks. The phase-2
delta query (`since=`, `created>=`) cost the same charged request for the
change and then a second charged `200` on the refresh after it, because the
moved bound made a new URL with no ETag; it also could not see deletions,
reactions or reruns of finished runs. It is gone ([DD-153](../DESIGN_DECISIONS.md#dd-153-merged-gh-reads-revalidate-every-page-instead-of-querying-a-delta)).
What the delta saved was bytes: a changed page is re-sent whole.

**Rendering.** The response is rebuilt from the joined pages, cut to the
caller's `per_page` (default 30), with the stored objects' exact bytes and
`total_count` from the first page. The rebuilt response drops `Link`,
`ETag`, `Last-Modified` and `Content-Length`, which describe one upstream
transfer.

**Exact or not at all.** A page is merged only if its parsed objects
re-render to exactly the bytes GitHub sent (no whitespace, no unknown wrapper
key, every object with an id and timestamps) and are in the natural order
above, since that is the order a caller's own fetch returns. A short page
that announces a next page is refused too. Otherwise the collection is
marked unmergeable for 30 minutes and the read takes the phase-1 exact-URL
path, as do the queries a merge cannot reproduce: `page` > 1, `per_page`
outside 1-100, `since`, `sort`, `direction`, a run-list `status` or `created`
filter, and any parameter the planner does not know. A collection over 10
pages, or a merged state over 8 MiB, is not merged either. Why the merge
keeps exact bytes: [DD-151](../DESIGN_DECISIONS.md#dd-151-merged-gh-reads-keep-exact-object-bytes-and-fall-back-rather-than-approximate).

**Freshness.** Within the TTL (comments 60 s, runs 30 s) a merged read costs
nothing. A write that names the collection, or a global invalidation, makes
the next read refresh at once, so `gh pr comment --delete-last` is reflected
immediately.

## Listings of finished runs and checks

`repos/{o}/{r}/actions/runs/{id}/jobs` and
`repos/{o}/{r}/commits/{sha}/check-runs` are ordinary exact-URL reads: after
their TTL they are revalidated with their ETag. Phase 2 froze them once
everything in them had finished and served them with no request until a
write in a session named them. Revalidating costs a free `304` while nothing
changed, the same as any check that could tell whether to thaw them, and it
also sees a rerun started outside clud and a check run another app adds to
a finished commit. So they no longer freeze.

## Freshness

| Endpoint | TTL |
| --- | --- |
| `.../actions/runs...`, `.../actions/jobs...` | 30 s |
| everything else | 60 s |

`CLUD_GH_FRESH=1` skips the TTL but still revalidates. Nothing is cached
forever, completed workflow runs and their job and check listings included:
`gh run rerun` reopens a completed run under the same id, and a `304`
revalidation is free, so every object has a TTL.

### Write invalidation

After any non-read `gh` call in a session, the shim posts `/gh/invalidate`.
That covers `gh api` with a non-GET method, a body flag or `graphql`, and any
porcelain subcommand that is not `view`, `list`, `checks`, `diff`, `status`,
`watch` or `download` (`classify::may_write`). `gh auth
login/switch/logout/refresh` count as writes too: they change the identity
behind the same env.

The broker records the time; a read counts as fresh only if its last
upstream request *started* after the newest invalidation that applies to it,
and in-flight fetches are detached so a later read cannot join one that
began before the write. A false positive costs one free `304`.

Phase 2 makes the invalidation targeted (`gh_broker::scope`). Every cached
read carries tags from its path:

| Read | Tags |
| --- | --- |
| `repos/{o}/{r}/issues/{n}...`, `.../pulls/{n}...` | `{o}/{r}#num:{n}` |
| `repos/{o}/{r}/actions/runs/{id}...` (the run, its jobs) | `run:{id}` |
| `repos/{o}/{r}/actions/runs` (list), `.../actions/jobs/...` | `{o}/{r}#runs` |
| `repos/{o}/{r}/commits/{ref}/check-runs` (and suites, statuses) | `{o}/{r}#checks` |
| anything else | `other` |

Each repo tag also comes as `*#...`. A write whose repo is unknown (no
`-R`/`--repo`, no `GH_REPO`, no URL selector) sends `*#...`, which matches
every repo. The shim's `scope::write_tags` names what a recognized write may
change, always plus `other`:

| Write | Tags |
| --- | --- |
| `pr merge <n>` | `num:{n}`, `runs` |
| `pr comment/close/reopen/edit/ready/review/lock/unlock <n>`, `issue comment/close/reopen/edit/lock/unlock/pin/unpin <n>` | `num:{n}` |
| `run rerun/cancel/delete <id>` | `run:{id}`, `runs`, `checks` |
| `gh api` write to `actions/runs/{id}/...` | `run:{id}`, `runs`, `checks` |
| `gh api` write to `pulls/{n}/merge` | `num:{n}`, `runs` |
| other `gh api` writes to a tagged path | that path's tags |

The selector must be the word right after the subcommand (`gh pr comment 5
--body x`), so a flag value is never mistaken for it. Every other write
(`pr create`, `workflow run`, `run rerun --job`, a `gh api` write to an
untagged path or a job, `graphql`, an unknown flag shape) sends no tags,
which is the phase-1 global invalidation. So a recognized write refreshes
every untagged read, as in phase 1, but leaves the tagged reads of other
issues, PRs and runs alone. An older daemon ignores the tags and invalidates
globally.

Writes made outside clud sessions (the web UI, CI, another machine) are seen
within one TTL: merged collections and listings alike, deletions, reaction
counts, outdated review comments, reruns of finished runs and check runs
added to a finished commit included. The one exception is `pr merge`
closing a linked issue, which leaves that issue's cached reads fresh for
their TTL.

## Subscriptions (phase 3)

`POST /gh/watch` (`gh_broker::service::watch`) lets a waiter block until
something it cares about changes, instead of polling GitHub itself. The
request names up to 16 REST endpoints, the identity (`gh`, forwarded env,
session), the digest of each body the waiter last saw, and how long to
wait (at most 55 s per call; a waiter calls again to keep waiting). The
daemon re-reads every endpoint through the broker's own read path, once a
second:

- Within an endpoint's TTL that is a cache hit: no request, no ledger row.
- After it, the broker revalidates: a free `304` while nothing changed. The
  refresh is single-flighted with every other subscriber and every ordinary
  read of the same key, so ten waiters on one PR cost what one does, and the
  waiter's next read of that key is a cache hit.
- A read that fails (an upstream error, nothing stored) is retried after one
  TTL, not every tick.

The call returns as soon as a digest differs from the one sent (`changed`
lists them), or when the wait runs out. A call without digests returns at
once with the current ones: the baseline. A failed read is `null` and never
counts as a change. Below the [floor](#rate-limit-floor-phase-3) the refresh
is deferred: the reply carries `deferred_until_s`, the digests are of stored
copies, and the call sleeps to the reset (or its deadline) in one step.

`pr_merge_watch.py` is the first waiter. In a session with the broker
(`CLUD_GH_READ_BROKER=1` and the daemon's `daemon.json`), it blocks on the
subscription between steady-state polls, at most six intervals (a heartbeat,
for GraphQL-only state such as review threads). Every watched key costs a
free revalidation per TTL, so it watches only the three reads that carry
verdicts:

- the PR: its state, head and mergeability, and its `updated_at` moves with
  review and comment activity;
- the head commit's check runs, at the exact URL its own `paginate` reads
  first, so the first baseline is of the body the poll just judged;
- the head commit's statuses.

A poll still decides everything, with the same exit codes: a required
failure exits 1 on the poll right after the wake, review activity exits 2,
an empty head exits 8. A poll whose verdict waits on a count or a clock (a
no-checks grace period, `mergeable=UNKNOWN` counted to exit 9, a CodeRabbit
wait, `--max-queued`) keeps the plain interval. Outside a session, or when
the daemon does not answer, the watcher polls as before.

Its REST reads are always conditional. In a session they are plain
`gh api` GETs the broker revalidates by ETag (the shim never brokers `-i`
or `-H`). Outside one, the watcher sends its own `If-None-Match` from an
in-process ETag cache (`api_get`). Its per-run progress report reads the run
and its jobs over REST instead of `gh run view --json jobs`, which is never
brokered.

## Rate-limit floor (phase 3)

`gh_broker::budget` keeps the newest `X-RateLimit-Limit`, `-Remaining` and
`-Reset` of the `core` resource for each identity, from every upstream
response, `304`s included. Concurrent responses arrive out of order, so
within one window the lower `remaining` wins, and an older window never
replaces a newer one.

Below the reserve (default 10% of the limit) and before the window resets,
a read that would go upstream is **deferred** unless it is interactive. The
shim marks a read interactive when its stdin is a terminal, so a person at
a prompt always reaches GitHub. A deferred read:

- is served the stored copy (TTL ignored) and marked stale: the ledger
  outcome is `deferred`, and the shim prints one line on stderr,
  `clud: gh read broker: rate-limit reserve (R of L requests left until
  <reset>): a cached copy from Ns ago, not a refresh`, while stdout is the
  stored body exactly as before;
- or, with nothing stored, goes to the real `gh` (a `409` from the route).
  The broker never invents a body.

A gate must not decide on a cached copy. `pr_merge_watch.py` counts the
marker in the stderr of its `gh` calls, and a poll that saw one neither
fails nor passes. It logs `api_degraded` with `reason=stale_reads` and waits
for fresh data. Subscriptions defer too ([above](#subscriptions-phase-3)).

The reserve is `CLUD_GH_BROKER_RESERVE_PCT` in the daemon's environment,
else `git.gh_read_broker_reserve_pct` in `~/.clud/settings.json` (re-read
at most once a minute), else 10. `0` turns the floor off; values over 100
mean 100.

## Store and ledger

`<state>/gh-broker.redb` (`~/.clud/state`, or `$CLUD_DAEMON_STATE_DIR`),
owned by the daemon:

| Table | Row |
| --- | --- |
| `object_meta` | key → label, status, headers, ETag, Last-Modified, `fetched_at_ms` |
| `object_body` | key → raw body (≤ 2 MiB; larger bodies are served, not cached) |
| `collections` | merged key → upstream pages (ETag, members' ids, `created_at` and exact bytes, `total_count`, headers), page width, `fetched_at_ms`, unmergeable mark |
| `scopes` | tag → stamp of the last write that named it |
| `meta` | `invalidated_at_ms` |
| `ledger` | seq → `ts_ms`, `session_id`, `key` (`host/endpoint`), `outcome`, `upstream_requests`, `rate_remaining`, `changed`, `removed` |

`outcome` is `cache` (no upstream request), `deferred` (below the
rate-limit floor: a stored copy, or the real `gh`, and no request), `304`
(for a merged read: every
page was a `304`), `incremental` (a merged read re-sent at least one page;
`changed` counts the objects added or changed, `removed` those deleted
upstream), `full` (an exact-URL fetch, or a merged collection's first fetch
or a re-fetch at a wider width), `passthrough` or `error`.
`upstream_requests` counts every request, free `304`s included. `GET /gh/ledger` on the daemon's
dashboard listener (capability cookie, like `/gh/read`) returns the newest
1,000 rows as JSON, oldest first. Objects are pruned oldest-first above 4,096, merged
collections above 1,024, tags above 4,096 (a pruned tag raises the global
stamp to its own, so pruning never makes a read fresher) and ledger rows
above 20,000. Calls the shim
never brokers are not in the ledger; they are in the per-invocation
`git-gh.jsonl` telemetry ([git-gh-telemetry-shim.md](git-gh-telemetry-shim.md)).
The design called for SQLite, but clud uses redb, not SQLite; see
[DD-150](../DESIGN_DECISIONS.md#dd-150-the-session-gh-read-broker-reruns-the-real-gh-over-a-loopback-replay).

## Setting

`git.gh_read_broker` in `~/.clud/settings.json` (default `true`, toggled
in `clud settings`). `shim_session::activate_rm` exports it at launch as
`CLUD_GH_READ_BROKER=1|0`; a running session keeps the value it launched
with.

A session launched before 2.8.24 has no key. Its `gh` alias runs the
current binary once the installed `clud` has relinked the shared alias
directory ([shim-dispatch.md](shim-dispatch.md#running-sessions-after-an-upgrade)),
and that alias then reads `git.gh_read_broker` itself (default on) instead
of treating the missing key as off. An explicit `CLUD_GH_READ_BROKER=0`
stays off. So after an upgrade such a session's next statusline tick or
`clud` call turns the broker on, with no restart
([DD-152](../DESIGN_DECISIONS.md#dd-152-a-running-sessions-aliases-are-relinked-by-the-installed-clud-not-self-delegated)).

### `gh.backend = "ghd"` (#2352)

A repo (or the user) can make [ghd](https://github.com/forge-ci/ghd) the
mandatory substitute for gh: `{"gh": {"backend": "ghd"}}` in clud's
layered settings (`repo_clud_config`, repo over user). The gh shim then
runs `ghd gh ARGS` for every call after the `pr checks --watch` upgrade.
`GHD_REAL_GH` names the real gh, so ghd's relay never resolves back to the
shim. ghd serves what it parses natively and relays the rest to that gh.
This broker is not consulted, because ghd keeps its own cache. The backend
is read from the call's cwd on every call, so a running session follows
the setting without a restart. With no ghd on PATH, dispatch prints one
line and exits 127; there is no silent fallback to gh. The contract is
pinned by the `shim_main::dispatch` test
`gh_backend_ghd_routes_to_ghd_and_fails_closed_without_it` (routing and the
missing-ghd refusal) and by `ghd_argv_prefixes_gh_and_preserves_every_arg`
(the `ghd gh ARGS` argv and the `GHD_REAL_GH` name).

## Code

| File | Role |
| --- | --- |
| `crates/clud-bin/src/gh_broker/classify.rs` | `api_read`, `may_write` |
| `crates/clud-bin/src/gh_broker/client.rs` | shim side: daemon request, replay server |
| `crates/clud-bin/src/gh_broker/service.rs` | TTL, single-flight, revalidation, the floor's deferrals, invalidation, ledger, `/gh/read` and `/gh/invalidate` bodies |
| `crates/clud-bin/src/gh_broker/service/watch.rs` | subscriptions: the `/gh/watch` body |
| `crates/clud-bin/src/gh_broker/budget.rs` | rate-limit windows per identity, the reserve |
| `crates/clud-bin/src/gh_broker/service/merged.rs` | merged reads: the conditional pass over every page, the re-check, fallback |
| `crates/clud-bin/src/gh_broker/collection.rs` | collection plans, page URLs and widths, page parsing, order checks, change counts, rendering |
| `crates/clud-bin/src/gh_broker/scope.rs` | invalidation tags of reads and writes |
| `crates/clud-bin/src/gh_broker/store.rs` | redb tables |
| `crates/clud-bin/src/gh_broker/upstream.rs` | `gh api -i` transport and parser |
| `crates/clud-bin/src/shim_main.rs` | `gh_shim::brokered_read`, invalidation after writes |
| `crates/clud-bin/src/shim_main/dispatch.rs` | builds the `BrokerClient` from the session env, or from the setting when the key is absent |
| `crates/clud-bin/src/daemon/http.rs` | `/gh/read` and `/gh/watch` (own threads), `/gh/invalidate` and `/gh/ledger` routes |
| `crates/clud-bin/assets/tools/github/pr_merge_watch.py` | `BrokerSubscription`, `broker_watch_keys`, `api_get`: the watcher's subscription and conditional reads |

Tests: `gh_broker` unit tests cover classification, TTL hits with zero
upstream requests, `304` revalidation, single-flight, invalidation, identity
separation, error passthrough, and the real `gh api -i` transport against a
fake `gh`. `service/phase2_tests.rs` runs merged reads against a fake GitHub
that answers `branch`, `per_page`, `page` and `If-None-Match` itself (its
ETag hashes the body, as GitHub's does), and compares every merged answer
byte for byte with that fake's full answer to the caller's exact URL: edits,
reactions and deletions on any page, reruns of finished runs in a run list
and a jobs listing, a late check run, a deletion that lands between two page
requests, run-list widths, page-size emulation, the exact-URL fallback and
targeted invalidation. `service/watch/tests.rs` runs subscriptions and the
floor on a fake clock: a quiet wait that costs nothing inside the TTL, a
change seen at the next refresh, a failed read that is not a change, stale
copies below the floor while interactive reads proceed, and a deferred
subscription that sleeps to its deadline. `tests/test_pr_merge_watch_subscription.py`
drives the watcher's client against a fake daemon and `watch()` against a
fake subscription, and checks that its env list mirrors `FORWARDED_ENV`.
`tests/test_gh_read_broker.py` drives the real alias against a
fake daemon and a fake `gh`. It checks byte-identical output, the fallbacks
(no daemon, a daemon miss, the setting off) and the invalidation each write
posts, and that a session without the key follows the setting.
