# Simorgh RPC contract — `simorghd` HTTP/JSON surface

This is the only boundary between the Simorgh GUI and the Simorgh engine.
It is the desktop/HTTP sibling of the mobile Kotlin↔Rust contract
(`native-contract.md` in the upstream ZeroNet-Mobile app): the method set,
request bodies, response bodies and job event schemas are the same, with the
differences listed at the end. The GUI talks to **nothing else** — not the
TUI, not JNI, not the engine's own management API.

* Server: the `simorghd` binary (crate `simorgh-daemon`).
* Transport: plain HTTP/1.1 on a **loopback** address. No TLS (the token
  covers a same-machine attacker; nothing else can reach the port).
* Everything crosses as **UTF-8 JSON** bodies. One request per connection
  (`Connection: close`); job event streams are the one connection that stays
  open until the job is done.

## Handshake

`simorghd` prints exactly one stdout line when — and only when — the RPC is
listening, then never writes to stdout again:

```text
SIMORGH_READY {"port":53123,"token":"6b1c9e2f0d4a4c88b7e5f1a2d3c4b5a6"}
```

The GUI spawns `simorghd --data-dir <dir> [--token-file <file>]`, reads that
line, strips the `SIMORGH_READY ` prefix, and parses the JSON. `--listen`
pins the port (default `127.0.0.1:0`, a random ephemeral port). Anything on
stderr before that line is a fatal start-up error.

## Authentication

Every request must carry:

```http
Authorization: Bearer <token>
```

The token is compared in constant time. A wrong or absent token is
`401 {"error":"unauthorized"}`. A non-loopback peer is refused even with the
right token (`403`). The daemon also enforces loopback at bind time: it
refuses to start with a non-loopback `--listen`.

## Error model

Uniform: method-level failures are **HTTP 200** with a body of
`{"error": "<message>"}` (the contract's "error string" shape). Transport-
level failures use real status codes:

| Status | Meaning |
|---|---|
| 400 | truncated request body |
| 401 | missing/bad bearer token |
| 403 | peer is not loopback |
| 404 | unknown route or unknown rpc method — body is `{"error": …}` |
| 413 | request body larger than 2 MB |

Within a 200 answer, `"error"` is present only on failure; success shapes are
per method below.

---

## `POST /rpc/<method>`

### `set_log_level`

Mirrors the mobile `setLogLevel(level): String?`.

```json
{"level": "debug"}        // request; one of off|error|warn|info|debug|trace
{"ok": true}              // answer
{"error": "unknown log level \"loud\""}
```

The initial level comes from `SIMORGH_LOG` (default `warn`). Logs go to
`<dataDir>/simorgh.log`, rotated to `simorgh.log.1` past 2 MB — the same
size cap as the mobile `zray.log`.

### `start`

Mirrors `start(configJson): String?`. The body wraps a **complete
engine configuration** — usually the output of `build_config`, byte-for-byte:

```json
{"config": { "inbounds": [...], "outbounds": [...], "routing": {...} }}
{"ok": true}
{"error": "the runtime is already started"}
```

`start` blocks until the listeners are bound (bounded at 10 s) before
answering. Two modes are supported and the daemon detects which one the
config asks for by the presence of an inbound with `"protocol": "tun"`:

* **proxy mode** — SOCKS/HTTP inbounds on loopback. No privileges needed.
* **vpn mode** — a `tun` inbound makes the engine open a macOS `utun`
  itself (the `zero-tun` path the desktop TUI uses). That needs root. The
  daemon never spawns an elevation UI; without sufficient privileges it
  answers immediately:

  ```json
  {"error": "requires root"}
  ```

  so the GUI can show its own prompt and re-run `simorghd` under `sudo`
  (or hand it a pre-opened descriptor... not part of this contract version).

  **One-time admin via stop-file.** A GUI that must not prompt twice launches
  the privileged daemon once with `--stop-file <shared>/STOP` (a directory the
  unprivileged GUI user also owns) and drives it over the same RPC at a fixed
  loopback port (the desktop app uses `127.0.0.1:37038`) with the token read
  from `--token-file`. To disconnect, the GUI simply creates the world-writable
  STOP file; the daemon polls for it every second, exits cleanly (stopping the
  engine on the way out), and needs no second prompt. Startup refuses if the
  stop-file directory is not writable — a daemon nobody can stop is worse than
  one that says so at start. A stale marker left over from a crashed session is
  cleared before the fresh daemon binds its port.

### `reload`

Mirrors `reload(configJson): String?`. Same body as `start`.

```json
{"ok": true}
{"error": "reload rejected: reload cannot change the inbound topology"}
```

Routing, DNS, outbounds and authentication switch atomically; open sessions
finish on the old generation. Inbound topology (which inbounds exist, their
ports, the TUN inbound) cannot change this way — stop and start instead.

### `stop`

Mirrors `stop(): String?`. `{}` request.

```json
{"ok": true}
{"error": "the runtime is not started"}
```

### `is_running`

Mirrors `isRunning(): Boolean`. `{}` request.

```json
{"running": false}
```

### `stats`

Mirrors `stats(): String?` — the contract's `String?` (null while nothing
runs) maps to `{"stats": null}`; while running, the inner object is exactly
the mobile `stats()` JSON:

```json
{"stats": {
  "race": null,
  "up": 1048576,
  "down": 5242880,
  "sessions": 7,
  "tun_lost": false,
  "tags": {
    "socks-in": {"up": 1234, "down": 5678},
    "proxy":    {"up": 1234, "down": 5678}
  },
  "cdn": "ok",
  "cdn_notice": null,
  "methods": [{"id": "fragment", "ok": true}]
}}
```

`up`/`down` are totals over every session the runtime carried (proxied and
direct); `sessions` is the number of live TCP sessions; `tags` holds
per-inbound and per-outbound byte counts keyed by tag (tags appear once they
have carried traffic). `{}` request.

### `network_changed`

Mirrors `networkChanged(): String?`. `{}` request.

```json
{"ok": true}
{"error": "could not reset for the new network: …"}
```

Re-installs the current configuration as a new generation: the resolver is
replaced, dropping the DNS cache and pooled DoH/DoT connections. Live
sessions are not killed. A successful no-op when nothing is running, as in
the contract. Call it when macOS reports a network switch (Wi-Fi ↔ cable).

### `build_config`

Mirrors `buildConfig(requestJson): String` → `{"config": …}` or
`{"error": …}`. The **BuildRequest object is the body itself** (no wrapper),
with the contract's schema verbatim:

```json
{
  "links": ["vless://...", "..."],
  "mode": "vpn",
  "tun": {"mtu": 1500, "ipv6": false},
  "socks_port": 10808, "http_port": 10809,
  "lan": {"enabled": false, "listen": "0.0.0.0", "user": "", "pass": ""},
  "iran_direct": true, "block_ads": true, "block_quic": true,
  "evasion": "off",
  "dns": {"remote": "google", "custom": "", "local": "google", "fakedns": true},
  "log_level": "warning",
  "clean_ips": ["104.16.1.2:443"]
}
```

Every field is optional except `links`; defaults are as shown except
`clean_ips` (empty) and `lan.enabled` (false). Semantics — inbounds,
balancer, routing rules, Iran-direct, QUIC blocking, evasion, `clean_ips`
as `clean_ip_candidates`, the managed-assets rule — are identical to the
mobile contract, because the builder is identical
(`zero_discovery::build_config_with_assets`). The one desktop difference:
managed geoip/geosite assets are looked for in **`<dataDir>/assets`**, the
daemon's own data dir.

```json
{"config": { …complete engine config… }}
{"error": "socks_port and http_port must be distinct and non-zero"}
```

The result always passes `zero_config::compile_config` before being
returned; when it would not, you get `{"error": <compiler message>}`.

### `parse_links`

Mirrors `parseLinks(text): String`.

```json
{"text": "vless://b831381d-...#DE\nss://Ym0...MTA=@1.2.3.4:8388#HK\nhttps://example.com"}
```

Answer — the contract's ParseReport, same fields:

```json
{"items": [
  {"key": "4d7c1e0a9b2f3610",
   "link": "vless://b831381d-...#DE",
   "name": "DE", "protocol": "vless", "transport": "xhttp", "security": "reality",
   "host": "example.de", "port": 443, "country": "DE",
   "class": "reality", "fp": null}
 ],
 "rejected": 0,
 "reasons": {},
 "duplicates": 0}
```

Accepted inputs are the contract's: plain lists, base64 subscription bodies,
links embedded in prose/HTML, several links per line. Non-proxy URLs are
ignored, not rejected. `key` is the first 16 hex digits of BLAKE3 over the
link without its `#remark`; `class` ∈ `xhttp_extra|reality|cdn|other`;
`fp` appears only on `warp://` links. `reasons` keys are
`<scheme>:<malformed|invalid|unsupported>`.

### `verify_signature`

Mirrors the mobile `verifySignature(publicKeyHex, body, signature): Boolean`
(used to authenticate crowd-data lists; an `ed25519:<hex>` line is the
signature format). Three arguments become one JSON object:

```json
{"public_key": "7d3085a1…", "body": "…list text…", "signature": "ed25519:…"}
{"valid": true}
```

### `built_in_public_key`

Mirrors `builtInPublicKey(): String`. `{}` request.

```json
{"key": "7d3085a128a20010febc485a344b586a132a454f2ab5e86c3c6cb5536db2cd62"}
```

Empty string means no key was compiled in; verification is then skipped by
the host rather than failed.

---

## Long-running jobs

Six methods, one route each. The request body is the contract's request JSON
**directly** (no wrapper). The answer is immediate:

```json
{"job": "3"}
{"error": "invalid discover request: …"}
```

`job` is an opaque string id (the mobile contract's numeric `Long` handle,
stringified). A job that cannot be constructed (unparseable request) returns
`{"error": …}` without an id — the mobile rule there was "handle 0 plus one
error event"; over HTTP the message arrives with no round-trip. Everything
else about jobs is unchanged: they run on a separate engine-owned runtime,
every value is clamped to sane ranges, and `done` is always the last event.

| Route | Request schema |
|---|---|
| `POST /job/discover` | `DiscoverRequest` (below) |
| `POST /job/test_links` | `TestRequest` |
| `POST /job/scan` | `ScanRequest` |
| `POST /job/warp_register` | `WarpRequest` |
| `POST /job/subscription_fetch` | `{"url": "https://…"}` |
| `POST /job/front_links` | `{"links": […], "seed": 0, "max": 18}` |

### `GET /job/<id>/events`

Newline-delimited JSON — one contract event per line, flushed as produced
(batching at most 50 ms, tighter than the mobile listener's 100 ms). Response
headers: `Content-Type: application/x-ndjson`, `Connection: close`.

The stream replays every event produced so far the moment you attach — so a
GUI that subscribes late misses nothing — then streams live, and the server
closes the connection after the terminal `done` event. One `done` line, then
EOF. Attaching twice is fine (two independent readers of the same log). If
the stream was fully consumed and the record has since been evicted (the
daemon keeps the last 512 finished jobs), you get
`404 {"error": "unknown job"}`.

### `POST /job/<id>/cancel`

Signals the job's cancellation token. `{}` body.

```json
{"ok": true}
{"error": "unknown job"}
```

Unknown or already-finished handles are ignored, as in the contract's
`cancel(handle)`.

## Job request schemas and event streams

Event shapes are copied faithfully from the mobile contract; field names and
values are identical.

### `discover` — `DiscoverRequest`

```json
{
  "sources": [{"id":"limilco","url":"https://raw.githubusercontent.com/.../new_configs.txt","tier":1}],
  "cache_dir": "/Users/me/.simorgh/cache/feeds",
  "priority_links": ["..."],
  "extra_links": ["..."],
  "exclude_keys": ["..."],
  "want_alive": 5, "max_seconds": 60,
  "tcp_concurrency": 256, "tcp_timeout_ms": 1500, "tcp_stop_after_open": 400,
  "real_concurrency": 24, "real_timeout_ms": 4000,
  "probe_url": "http://cp.cloudflare.com/generate_204",
  "confirm_tls": true,
  "next_tier_if_alive_below": 3,
  "fetch": true,
  "fetch_timeout_ms": 20000
}
```

All fields optional (defaults as shown; `sources`, the link lists and
`exclude_keys` default to empty). **Daemon default:** an absent `cache_dir`
is filled with `<dataDir>/cache/feeds`, so the GUI does not have to know the
layout. The algorithm (tiered feeds → parse → dedupe by `key` → class
round-robin → TCP stage → real stage with verified-TLS confirmation → stop
at `want_alive`/exhaustion/`max_seconds`/cancel) is the contract's,
unchanged.

Events (each line one object):

```json
{"t":"stage","stage":"history"}
{"t":"stage","stage":"fetch"}
{"t":"source","id":"limilco","status":"ok","count":123,"bytes":45678}
{"t":"source","id":"old","status":"error","count":0,"bytes":0,"error":"fetch failed: connection reset"}
{"t":"stage","stage":"parse"}
{"t":"stage","stage":"tcp"}
{"t":"progress","candidates":4200,"tcp_done":800,"tcp_open":230,"real_done":60,"alive":2}
{"t":"stage","stage":"real"}
{"t":"alive","info":{"key":"4d7c1e0a9b2f3610","link":"vless://…","name":"DE","protocol":"vless","transport":"xhttp","security":"reality","host":"…","port":443,"country":"","class":"reality"},"delay_ms":312}
{"t":"done","alive":5,"reason":"enough","elapsed_ms":8123}
{"t":"error","message":"…"}
```

* `stage` ∈ `history|fetch|parse|tcp|real`, as in the contract.
* `source.status` ∈ `ok|cached|not_modified|error`; `error` is present only
  when the status is not `ok`.
* `progress` is emitted on a 500 ms tick only when a counter changed, plus
  once more right before `done`.
* `done.reason` ∈ `enough|exhausted|timeout|cancelled`. `done` is always the
  last event.
* An `alive` key is reported once even when several feeds list it.

### `test_links` — `TestRequest`

```json
{"links":["..."],"concurrency":16,"timeout_ms":4000,"probe_url":"http://cp.cloudflare.com/generate_204","tcp_only":false,"confirm_tls":true}
```

Events:

```json
{"t":"result","key":"4d7c1e0a9b2f3610","delay_ms":312}
{"t":"result","key":"9f2a3b4c5d6e7f80","delay_ms":-1,"error":"parse: unsupported scheme"}
{"t":"done","alive":1,"tested":2,"reason":"exhausted","elapsed_ms":2}
```

Every link produces exactly one `result`, an unparseable link too (keyed by
the same hash, with an error starting `parse:`). `tcp_only` times the TCP
handshake instead; for hysteria2/tuic it reports an error. `done.reason` ∈
`exhausted|cancelled`.

### `scan` — `ScanRequest`

```json
{"preset":"cloudflare","ports":[443,2053,8443],"host":"www.speedtest.net","count":2000,"concurrency":128,"timeout_ms":1500}
```

Events:

```json
{"t":"progress","scanned":512,"responsive":6,"total":2000}
{"t":"ip","ip":"104.16.1.2","port":443,"rtt_ms":82}
{"t":"done","scanned":2000,"responsive":19,"reason":"exhausted","elapsed_ms":183442}
```

Only `preset:"cloudflare"` exists; anything else is one `error` event
followed by `done`, exactly as on mobile. TLS mode (TCP connect + handshake
with SNI `host`), IPv4 only, `count` split across ports, neighbours of
responsive addresses probed next.

### `warp_register` — `WarpRequest`

```json
{"proxy":"127.0.0.1:10809","direct":true,"want":4,"sample":100,"budget_ms":60000}
```

Every field optional. Events:

```json
{"t":"step","line":"registering a WARP account…"}
{"t":"done","ok":true,"link":"warp://…","exits":3,"route":"auto","fingerprint":"3FA9 C0D1 7B42 E8A5 11C2 D3E4 F5A6 7890 AB12 CD34 EF56 7890 1234 5678 9ABC DEF0"}
{"t":"done","ok":false,"error":"…"}
```

`link` holds private keys: import it, never log it. `fingerprint` (eight
groups of four hex digits) is safe to show. Keys are made on this machine;
only public halves leave it.

### `subscription_fetch`

Mobile exposes this as a synchronous native
(`subscriptionFetchUrl(address)`); the daemon keeps it in the job family for
a uniform long-work surface (see Deviations). Request:

```json
{"url": "https://bash.page/…"}
```

Events — one result, then the terminal `done`:

```json
{"t":"result","url":"https://bash.page/sub?markdown=false&include_target=true"}
{"t":"done","reason":"exhausted","elapsed_ms":0}
```

A BPB panel address is rewritten to its smallest equivalent; anything else
comes back unchanged. The URL is never logged: a panel's path is its
password.

### `front_links`

Also synchronous on mobile (`frontLinks(requestJson)`). Request:

```json
{"links": ["vless://…security=tls&type=ws…", "…"], "seed": 0, "max": 18}
```

Each CDN-fronted TLS link is re-aimed at a bounded sample of Cloudflare edge
IPs (SNI and Host kept); links that cannot be fronted (REALITY, plain
TCP-TLS, no TLS) contribute nothing. `max` is capped at 64 — every variant
is a probe. Events:

```json
{"t":"result","links":["vless://…host=104.16.1.2…","…"]}
{"t":"done","reason":"exhausted","elapsed_ms":0}
```

---

## Typical GUI session

```bash
DIR=$(mktemp -d)
./simorghd --data-dir "$DIR" &     # wait for SIMORGH_READY, capture port+token
TOK=…; PORT=…
curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d '{}' "http://127.0.0.1:$PORT/rpc/is_running"          # {"running":false}
curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d '{"text":"vless://…"}' "http://127.0.0.1:$PORT/rpc/parse_links"
JOB=$(curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d '{"links":["vless://…"],"tcp_only":true}' \
     "http://127.0.0.1:$PORT/job/test_links" | jq -r .job)
curl -Ns -H "Authorization: Bearer $TOK" \
     "http://127.0.0.1:$PORT/job/$JOB/events"                 # NDJSON until done
CFG=$(curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d '{"links":["vless://…"],"mode":"proxy"}' \
     "http://127.0.0.1:$PORT/rpc/build_config" | jq .config)
curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d "{\"config\":$CFG}" "http://127.0.0.1:$PORT/rpc/start"
curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d '{}' "http://127.0.0.1:$PORT/rpc/stats"
curl -s -X POST -H "Authorization: Bearer $TOK" \
     -d '{}' "http://127.0.0.1:$PORT/rpc/stop"
```

## Deviations from `native-contract.md` (and why)

1. **Envelope for `String?` results.** The mobile contract's `null`/error
   string becomes `{"ok": true}` / `{"error": "…"}` on
   `start|reload|stop|network_changed|set_log_level`, and
   `{"stats": null}` / `{"running": false}` for the nullable returns — HTTP
   answers must be JSON objects with a stable shape.
2. **Job handles are strings** (`{"job": "3"}`), because JSON ids are
   opaque; they are the same monotonic numbers the mobile `Long` handles
   are.
3. **Invalid job request bodies** answer `{"error": …}` immediately instead
   of "handle 0 + one error event": no event stream can attach to a job that
   never existed, so the message is delivered at the call that failed.
4. **`subscription_fetch` and `front_links` are jobs.** On mobile they are
   synchronous natives; the daemon runs them on the job runtime with a
   `{"t":"result",…}` + `{"t":"done",…}` stream so the GUI has exactly one
   async pattern for host-driven work. (`verify_signature` and
   `built_in_public_key` stayed synchronous — they are pure and fast.)
5. **`verify_signature` takes one JSON object** where JNI takes three
   arguments (HTTP has no argument list).
6. **VPN mode on macOS** opens a `utun` directly through `zero-tun` (the
   TUI's desktop path), which needs root; the daemon answers
   `{"error": "requires root"}` without touching an elevation UI, where the
   mobile contract has no such case (the OS owns the tunnel there).
7. **`discover` fills `cache_dir`** with `<dataDir>/cache/feeds` when the
   request omits it (mobile always supplied its `filesDir` path explicitly).
8. **Event batching is 50 ms**, half the mobile 100 ms ceiling — the stream
   is read by an HTTP client, not by a UI thread the way JNI callbacks are.
9. **Socket protection is a no-op**, installed once at start-up: a desktop
   process is not behind its own tunnel. The contract's `protect(fd)` JNI
   round-trip has no desktop equivalent.
10. **Logging** goes to `simorgh.log` (rotated at 2 MB) with the level from
    `SIMORGH_LOG`, mirroring `init(dataDir, logLevel)`; there is no second
    sink like Android's logcat.
