# Task brief - Zap custom transport (native-to-native "fast lane")

> Self-contained handover + initiation brief for the **custom transport** feature.
> Written so a fresh agent (new chat, no prior context) can pick it up and build
> it. General project context is in `docs/HANDOFF.md`; repo layout is in
> `docs/restructure-handoff.md`.
>
> **Status: DESIGN AGREED, NOT STARTED.** Nothing in this brief is implemented
> yet. Build it in phases (§7) and verify each on real devices.

---

## 0. TL;DR

Add a second, optional transport to Zap - a **custom protocol over TCP**, used
**only when a native Zap app is on both ends** - to push throughput and
resilience on flaky-but-working Wi-Fi. The **HTTP(S) browser path stays the
default and is never removed**: it is the "no app on the receiver" superpower and
a browser physically cannot speak a raw custom protocol. So this is **additive**,
not a replacement. Think of it as dual-transport:

| Path | Who uses it | When |
| --- | --- | --- |
| **HTTP(S) browser path** (exists today) | any device, just a browser | default, universal |
| **Custom TCP transport** (this task) | Zap app on BOTH ends | negotiated, opt-in fast lane |

Honest scope: this helps on **lossy-but-connected** links (a single TCP stream
collapses under loss/latency; multiple adaptive streams fill the pipe far
better). It does **not** beat RF physics - a marginal double-Wi-Fi link or a
phone dropping Wi-Fi entirely is not a protocol problem (see the reliability
findings in `docs/HANDOFF.md`). Set that expectation in any messaging.

---

## 1. Why (the hard constraint that shapes everything)

Zap's whole pitch is **"the receiver opens a link in a browser - no app."** A web
browser speaks HTTP/HTTPS/WebSocket/WebRTC/WebTransport only; it cannot open a
raw custom TCP protocol. Therefore:

- The custom transport can only run **app-to-app** (both ends are native Zap:
  Android app, desktop app, or CLI).
- The **HTTP server must keep running** exactly as today so browser receivers
  keep working. The custom transport is a *separate listener* the native client
  discovers and uses; if it is unavailable or fails, the client **falls back to
  the HTTP path**.

Breaking or gating the browser path is a non-goal and a regression. Guard it.

---

## 2. Current state (the baseline you build on)

Repo: `git@github.com:sodatta119/zorigin.git`, branch `main`. Cargo workspace
under `networking/` (category layout). Shared engine crate = **`znet-core`**
(`networking/crates/znet-core`), lib name `znet_core`. Front ends: `zap-cli`,
`zap-desktop` (egui), `zap-android` (JNI cdylib, `libzap_android.so`), Android
Gradle project at `networking/android/zap/`.

The web transport lives in `networking/crates/znet-core/src/web/mod.rs`. Public
API you will extend/reuse:

- `struct ServeConfig { dir, port, bind, auth: Option<Credentials>, history:
  Option<PathBuf>, index_html: Option<String>, tls: Option<TlsMaterial> }`
- `fn serve(config, on_ready)` (blocking) and `fn spawn(config) -> (ServerInfo,
  ServerHandle)` (non-blocking; the Android service + desktop use this).
- `struct ServerInfo { dir, port, lan_ip }`, `.url()`, `.url_with_key()`
  (appends `?k=<token>` - the pairing key / session token used for auth).
- `ServerHandle`: `transfers()`, `bytes_transferred()`, `requests_seen()`,
  `remove_transfer(id)`, `clear_transfers()`, `events() -> EventHub`, `stop()`.
- **TLS already exists** (`web/tls.rs`, feature `tls`): `tls::self_signed(sans)
  -> TlsMaterial { cert_pem, key_pem, fingerprint }` and `fingerprint_hex(der)`.
  `ServeConfig::tls` makes the HTTP server speak HTTPS with a pinned self-signed
  cert. Reuse this for encrypting the custom transport.
- CRC-32 (IEEE, table-driven) helpers in `mod.rs` (`crc32_file`, `crc32_table`,
  and the JS `crcUpdate` in `web/index.html`) - reuse for integrity.
- Resumable upload/download over HTTP already works: chunked `Range` GET with
  per-chunk retry (client in `web/index.html` `downloadFile`), `HEAD /upload`
  offset + `PUT ...&offset=` temp-file append, `ETag`/`If-Range`, coalesced into
  one transfer per file (`begin_upload` / `begin_download`, `DownloadReader`).
  The custom transport should mirror these semantics (offset resume + integrity).

Android JNI bridge (`networking/crates/zap-android/src/lib.rs`, declared in
`NativeBridge.kt`): `nativeStart(dir,port,user,pass,history)`, `nativeUrl`,
`nativeShareUrl`, `nativeTransfers`, `nativeRequests`, `nativeRemoveTransfer`,
`nativeClearTransfers`, `nativeStop`. You will add native entry points for the
client side of the custom transport (see §4).

Phone-server reliability already shipped (keep it): foreground service +
WifiLock + WakeLock + `requestNetwork(TRANSPORT_WIFI)` + `FLAG_KEEP_SCREEN_ON`
(in `ZapService.kt` / `MainActivity.kt`).

---

## 3. Design

### 3.1 Transport base - TCP, multi-stream (not raw/UDP)

Use a **custom application-layer protocol over TCP**. TCP already gives reliable,
ordered delivery, so do NOT reimplement retransmission or add forward-error-
correction (FEC over TCP is meaningless - TCP guarantees delivery). The real win
on flaky Wi-Fi is **multiple parallel TCP connections**: a single stream's
congestion control collapses under loss/latency, while N streams pulling
different byte ranges keep the aggregate pipe full. So the adaptive levers are:

- **number of parallel connections** (start ~4, scale by measured aggregate
  throughput),
- **chunk / range size per request** (tune by RTT and throughput),
- **pipelining depth** (how many ranges in flight per connection).

*(If you later want true loss-tolerant transport - independent streams, no
head-of-line blocking, real FEC - that is QUIC/UDP, e.g. the `iroh` or `quinn`
crates. Out of scope here; the owner asked for a custom TCP protocol. Note it as
a future option, do not build it now.)*

### 3.2 Discovery + negotiation (HTTP bootstraps the fast lane)

HTTP stays the control channel. Add:

- `GET /api/capabilities` -> JSON, e.g.
  `{"fast":{"port":<p>,"tls":true|false,"version":1}}`. Absent/`fast:null` means
  the server has no fast lane (older build) -> client uses HTTP.
- The server opens the custom-transport listener on a separate port (e.g.
  `http_port + 1`, or an OS-assigned port advertised in capabilities), bound to
  the same interface.
- A native client that opens a Zap URL first calls `/api/capabilities`; if a fast
  lane is advertised **and reachable**, it uses it; otherwise it falls back to the
  existing HTTP download/upload path. Browsers never call this - they just render
  the page - so they are unaffected.

### 3.3 Wire protocol (custom)

Keep it small and versioned. Suggested framing (binary, little-endian):

- **Handshake (client -> server):** magic `b"ZAPX"` + `u16 version` + auth token
  (the pairing key / session token from `url_with_key`, empty if server is not
  secured) + op (`GET`=download / `PUT`=upload) + path (len-prefixed UTF-8) +
  `u64 offset` (resume point) + `u64 range_len` (0 = to EOF).
- **Handshake reply (server -> client):** status + `u64 total_size` + optional
  `u32 crc32` (whole-file, for downloads) + error string on failure.
- **Data:** raw bytes for the requested `[offset, offset+range_len)`; because
  each connection requests a distinct range, the client writes each into the
  correct file offset and reassembles. No per-byte framing needed inside a range
  (TCP is a reliable stream); optionally a trailing per-range CRC for cheap
  corruption detection.
- **Control (optional, v2):** a small side message for the client to report
  measured throughput so the server can hint chunk size. v1 can let the client
  drive all adaptation locally.

Auth: require the token to match when the server is in Secure mode (same model as
HTTP session/pairing key). Reject otherwise. TLS: if `ServeConfig.tls` is set,
run the custom listener over the same rustls cert (native clients pin the
fingerprint, which they already learned via the QR/pairing).

### 3.4 Integrity + resume (mirror the HTTP path)

- **Downloads:** client requests ranges, writes to a temp file at the right
  offsets, and on completion verifies assembled size == `total_size` and (if the
  server sent one) the whole-file CRC-32 before renaming into place. Never expose
  a partial file as complete (this is the core correctness rule Zap already
  enforces on HTTP).
- **Uploads:** append to `.zap-part-<name>` at a verified offset, atomic rename +
  CRC-32 verify on completion - identical to the HTTP resumable upload. Reuse
  `begin_upload` / `begin_download` so the Transfers UI shows ONE coalesced row.
- **Resume across drops:** on connection loss, re-handshake from the current
  on-disk offset. Same "temp file size is the checkpoint" model as HTTP.

### 3.5 Adaptation (the "reads your link and tunes itself" part - make it real)

- Measure per-connection throughput and RTT (bytes/elapsed; TCP connect + first-
  byte time).
- Maintain a target concurrency: increase streams while aggregate throughput
  rises with each added stream; back off when it plateaus or the link degrades.
- Size ranges from RTT x throughput (keep enough in flight to fill the pipe
  without giant re-fetches on a drop). Keep ranges modest (e.g. 1-8 MB) so a
  dropped stream re-fetches little.
- This is genuine, measurable behavior - not marketing. Log the chosen
  concurrency / chunk size so it is observable in tests.

### 3.6 Fallback + safety

- If the fast lane is not advertised, not reachable (firewall / AP isolation), or
  errors mid-transfer beyond retries, **fall back to the HTTP path** for the rest
  of the file (resume by offset - the bytes already on disk are reused).
- Never let a fast-lane failure fail the whole transfer if HTTP could finish it.

---

## 4. Implementation plan (phases - each independently shippable + verifiable)

- **P0 - capabilities + design note (S).** Add `GET /api/capabilities`. Write a
  1-page wire-format spec in this repo. Accept: browser path unchanged; endpoint
  returns fast-lane info.
- **P1 - server listener + single-stream download (M).** Custom listener on a
  second port; handshake; serve one byte range over one connection. Native client
  (start in `zap-cli` for easy testing: `zap get <url> <dest>`) downloads a file
  via the fast lane, byte-exact, with HTTP fallback. Accept: CLI fast-lane
  download of a large file, byte-exact; kill server mid-transfer -> resumes or
  falls back.
- **P2 - multi-stream parallel ranges + reassembly (M).** N connections, distinct
  ranges, reassemble to temp file, verify size + CRC, atomic rename. Accept:
  measurably higher throughput than single-stream on an induced-loss link
  (e.g. `tc netem` on Linux, or a real weak-Wi-Fi test).
- **P3 - adaptive concurrency + chunk sizing (M).** Live measurement drives
  stream count + range size. Accept: logs show adaptation; throughput within a
  reasonable fraction of link capacity on a lossy link.
- **P4 - uploads over the fast lane + TLS (M).** Resumable, CRC-verified uploads;
  run the listener under the existing rustls cert; token auth. Accept: app-to-app
  upload byte-exact + verified; encrypted; wrong token rejected.
- **P5 - wire into apps + UX (M).** Native client in `zap-desktop` and
  `zap-android` (JNI): auto-detect the fast lane when connecting app-to-app, use
  it, show a subtle "turbo / direct" indicator, and fall back silently otherwise.
  Rebuild dist + APK. Accept: two Zap apps on the same LAN transfer over the fast
  lane end-to-end; a browser receiver still works unchanged.

Keep every phase behind graceful fallback so partial progress never breaks the
shipped HTTP experience.

---

## 5. Where the code goes

- **Core (both server + native client):** new module in `znet-core`, e.g.
  `networking/crates/znet-core/src/fast/` (`mod.rs` server listener + protocol,
  `client.rs` native client). Spawn the listener from `spawn()`/`serve()` when
  enabled (add `ServeConfig { fast: bool }` or always-on with capability
  advertise). Reuse `crc32_*`, the auth token, `tls::*`, and the `Stats` /
  `begin_upload` / `begin_download` transfer bookkeeping.
- **CLI (test harness first):** `networking/crates/zap-cli` - add a client
  command so the fast lane is testable without the GUIs.
- **Desktop:** `networking/crates/zap-desktop/src/main.rs` - use the client when
  connecting to another Zap; show the indicator.
- **Android:** `networking/crates/zap-android/src/lib.rs` (new JNI entry points) +
  `NativeBridge.kt` decls + `MainActivity.kt` UI. Rebuild `.so` via cargo-ndk.
- **HTTP `/api/capabilities`:** in `web/mod.rs` router (next to `/api/list`).
- **Browser page:** unchanged (browsers do not use the fast lane).

---

## 6. Key decisions + non-negotiables

- **Never remove or gate the HTTP browser path.** It is the differentiator and
  the only path a browser can use. The fast lane is additive + optional.
- **TCP multi-stream, not FEC/UDP.** FEC over TCP is pointless. If cross-network
  or true loss-tolerance is ever needed, that is a separate QUIC/`iroh` track.
- **Reuse, do not reinvent:** CRC-32, the pairing token, rustls TLS, and the temp-
  file resume + coalesced-transfer bookkeeping already exist. Match their
  semantics so the Transfers UI, resume, and integrity behave identically.
- **Honest capability:** the fast lane helps lossy-but-connected links; it does
  not fix a broken RF link or a phone that drops Wi-Fi (that is the keep-radio-
  awake work already shipped). Do not oversell.
- **Verify end-to-end on real devices** (owner has a USB-connected MIUI phone -
  note: MIUI blocks `adb shell input`, so drive the app by hand; the service is
  not exported). Rebuild `dist/` + reinstall the APK after changes so the owner
  can test. See `docs/HANDOFF.md` §5-§7 for build/run/gotchas.

---

## 7. Open questions (decide with the owner before/while building)

1. **Fast-lane port:** fixed `http_port+1`, or OS-assigned + advertised? (Advertised
   is more robust behind port constraints.)
2. **Always-on vs opt-in:** advertise the fast lane by default, or behind a
   setting? (Recommend on by default with silent fallback.)
3. **TLS for the fast lane:** required, or only when the user enables "encrypted"?
   (Recommend: follow `ServeConfig.tls`; if HTTP is plain, fast lane can be plain
   on-LAN, token-authed.)
4. **CLI client command name / shape** (`zap get`, `zap pull`?).
5. **Turbo indicator UX** in desktop/Android (how prominent?).

---

## 8. Working conventions (carry over - see `docs/HANDOFF.md` §6)

- **Single hyphen only.** Never an em dash or en dash anywhere (code, docs,
  commits, UI). The owner asked for this repeatedly.
- **Commit directly to `main` and push** (no feature-branch ceremony); the CI
  `release.yml` only builds installers on a `v*` tag, so normal pushes are safe.
- **Rebuild + redeploy after non-trivial changes:** `./scripts/build-dist.sh`
  (universal macOS dmg/cli into `dist/zap/`), and for Android rebuild the `.so`
  (`cargo ndk ... -o networking/android/zap/app/src/main/jniLibs`) + APK
  (`gradlew :app:assembleDebug`) + `adb install -r`, so the owner can test.
- **Do it in coherent phases, verify each, do not iterate symptom-by-symptom.**
- Tests live in `znet-core` (`cargo test -p znet-core --lib`, 30 green today) -
  add protocol/round-trip/resume tests as you build.

---

## 9. Definition of done

- Two Zap apps on the same LAN transfer a large file over the custom TCP
  transport, byte-exact + integrity-verified, with measurably better
  throughput/resilience than the single-stream HTTP path on a lossy link.
- A browser-only receiver (iPhone/Android/desktop) still transfers exactly as
  today - no regression, no app required.
- Any fast-lane failure falls back to HTTP and still completes (resumed by
  offset). Interrupted transfers never surface as complete.
- `znet-core` tests cover handshake, multi-range reassembly, resume, and
  integrity. dist + APK rebuilt; owner-verified on device.
