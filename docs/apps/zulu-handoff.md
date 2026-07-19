# Zulu - technical handoff

> Pick-up-and-continue brief for **Zulu** (zOrigin networking product #2:
> clipboard / link / snippet sync). Written 2026-07-19, as of commit `f57e6c4`
> on `main`. Read this first, then `docs/apps/zulu-brief.md` (the original build
> brief) and the files it points to.

## 1. What Zulu is (one paragraph)

**Zulu = continuous clipboard / link / snippet sync across your paired devices.**
Copy on one device, it shows up on the others. It reuses Zap's engine
(`znet-core`): one device **hosts** an HTTP server on the LAN, the others connect
(native app or any browser). The new piece Zulu added to the core is **live
push** - a Server-Sent-Events stream so the host can push a clip to every device
the instant it's copied. Same family promise: **no cloud, no accounts, data stays
on the LAN, explicit QR/URL/token pairing - never mDNS/BLE/discovery.** Not file
transfer (that's Zap).

The make-or-break constraint (design *around* it, never pretend it's solved):
mobile OSes **block background clipboard reads**, and browsers need a user
gesture to **write** the clipboard. So: **desktop ↔ desktop is fully automatic;
mobile is assisted** (share-sheet to send, tap-to-copy to receive). Say this
plainly - the audience includes networking pros.

## 2. Current state - what's shipped (all on `main`)

Everything below is built, tested, and pushed.

**Core primitives in `znet-core` (`networking/crates/znet-core/src/web/`):**
- **`events.rs`** - the family's live-push/presence primitive: `EventHub`,
  `Event`, `SseReader`. `GET /events` holds open an SSE stream; the host
  broadcasts `clip` and `presence` frames. `subscribe_with_backfill` replays
  recent clips to a just-connected device (no client-side JSON parsing).
  `serve_events` takes the raw writer and **flushes per frame** (plain
  `Response` buffers, which would stall a never-ending stream). Reachable via
  `ServerHandle::events()`.
- **`clips.rs`** - `ClipStore` (capped ring, 50) + `Clip`. `POST /clip` (store +
  broadcast), `GET /clips` (backfill). Body cap `MAX_CLIP_BYTES = 8 MiB` (fits a
  small image's base64).
- **`mod.rs`** - `ServeConfig` gained `index_html` (an app supplies its own SPA
  for `/`, keeping the server generic) and `tls: Option<TlsMaterial>`.
  `ServerInfo` gained `tls_fingerprint`; `url()/url_with_key()` switch to
  `https://` + `&fp=` under TLS.
- **`tls.rs`** (Cargo feature `tls`, off by default) - `self_signed()` (rcgen) +
  `fingerprint_hex()` (sha2). Serves HTTPS through tiny_http's rustls backend.

**Desktop app - `zulu-desktop` (`networking/crates/zulu-desktop/src/`):**
- **`main.rs`** - egui shell (Zulu-blue theme, `tune_theme`), **Host** and
  **Join** modes, **Encrypt (TLS)** toggle, **pinned snippets** UI (persist to
  `<app-data>/zulu/pins.txt`), origami-giraffe icon. Screenshot/test env hooks
  (see §5).
- **`sync.rs`** - the engine. A **receiver** thread holds `GET /events` and
  writes incoming clips to the OS clipboard (`arboard`); a **sender** thread
  polls the clipboard and `POST`s changes to `/clip`. Echo loop broken by a
  content guard + clip-id dedup + a short post-image-apply mute.
- **`tlsclient.rs`** - `Conn` (plaintext | pinned-TLS) behind `Read`/`Write`;
  `rustls` **ring** provider with a SHA-256 fingerprint-pinning verifier.
- **`imageclip.rs`** - small-image sync: encode/decode `data:image/…;base64,…`,
  dependency-free base64. Encodes **PNG**, decodes **PNG + JPEG**. `MAX_DIM 2560`,
  `MAX_PNG_BYTES ~4 MB`.
- **`zulu.html`** - the **no-app web receiver** served at `/`: EventSource clip
  list (+ backfill), presence, a paste-and-send box, tap-to-copy (with an
  `execCommand` fallback for plain-http LAN), inline image rendering.

**Android app - `networking/android/zulu/`** (pure Kotlin, no JNI, no service):
- `MainActivity` - pair (save the host URL) + open the web receiver.
- `ShareActivity` - the system **share target**: `text/plain` **and `image/*`**.
  Text/links POST directly; images go through `ImageClip.kt` (downscale ≤2560,
  JPEG q85, data URL) then POST to `/clip`.
- `Net.kt` - host-URL prefs + `Clip.send` (plain `HttpURLConnection`).
- Origami-giraffe adaptive launcher icon.

**Distribution:**
- `scripts/build-dist.sh` - builds into **`dist/<product>/`**: universal macOS
  `.dmg` (+ CLI for Zap) and the Zulu `.apk` (best-effort, if an Android SDK is
  found). On Linux, `.deb`s.
- `.github/workflows/release.yml` - on a `v*` tag, `macos`/`windows`/`linux`/
  `android` jobs build Zap+Zulu `.dmg`/`.zip`/`.deb` and the Zulu `.apk` into the
  same `dist/<product>/` layout as GitHub Release assets.

## 3. How the pieces talk (the sync loop)

```
copy on A ──▶ A.sender polls clipboard ──▶ POST /clip ──▶ host ClipStore + EventHub
                                                                   │ broadcast
                    GET /events (held open) ◀──────────────────────┤
   B.receiver applies to OS clipboard ◀── clip frame ── every connected device
```
- Both Host and Join run the **same** `sync.rs` engine against a base URL; Host
  just also runs the server and points its own client at `127.0.0.1`.
- Presence: connect/disconnect broadcasts `event: presence {"count":N}`.
- Backfill: a new device gets recent clips as its first frames.
- Images ride the identical path as a `data:` URL string.

## 4. Build / run / verify

From `networking/` (the Cargo workspace):
```sh
cargo test                              # workspace: znet-core 27, zulu-desktop 10, ...
cargo test -p znet-core --features tls  # + the TLS pinning test
cargo run -p zulu-desktop               # the desktop app
```
- **Two-machine test:** run `zulu-desktop` on two machines on the same Wi-Fi -
  one **Host**, one **Join** (paste the Host's URL). Copy on one, watch it land
  on the other. Any phone/laptop **browser** can also open the Host URL for the
  no-app receiver.
- **Android APK:** `cd networking/android/zulu && ANDROID_HOME=<sdk> ./gradlew
  :app:assembleDebug` -> `app/build/outputs/apk/debug/app-debug.apk`.
- **Distributables:** `./scripts/build-dist.sh` -> `dist/zap/…` + `dist/zulu/…`
  (dmg + apk locally; Windows `.zip` / Linux `.deb` come from CI on a `v*` tag).

## 5. Test / debug env hooks (zulu-desktop)

- `ZULU_SHOT=<path>` - render one frame to a PNG and exit (GUI screenshot
  harness, mirrors Zap's `ZAP_SHOT`).
- `ZULU_SHOT_RUNNING=1` - also auto-start hosting before the screenshot.
- `ZULU_AUTOHOST=1` - auto-start Host mode on launch (headless-drivable).
- `ZULU_SECURE=1` - with AUTOHOST, host over TLS.
- `ZULU_DARK=1` - force the dark theme.

## 6. Conventions (carry over)

- **Commit directly to `main`** and push. (This work was developed in a Claude
  git *worktree* under `.claude/worktrees/…`; the worktree is not part of the
  repo - only commits/pushes matter. `dist/` is gitignored, so build artifacts
  are produced per-checkout, never committed.)
- **Single hyphens only** - no em/en dashes anywhere.
- **Honesty over hype** - state OS limits plainly; never overclaim.
- **Verify end-to-end**, not just unit tests - drive the real flow (browser,
  real clipboard via `pbcopy`/`pbpaste`, two devices).
- TLS crypto uses the **`ring`** provider (not aws-lc-rs) so CI builds on Windows
  without nasm/cmake. The `image` crate is `png`+`jpeg` only.

## 7. Open / next

- **Play Store**: signed AAB, `image/*`-share notwithstanding, data-safety form,
  closed testing (this mirrors Zap's Horizon-0 launch items).
- **Browser over TLS**: a self-signed host triggers browser cert warnings, and
  the browser can't *write* an image to the clipboard on plain http. A trusted
  cert story (or native-only encryption) is the follow-up. TLS today is
  **native ↔ native**.
- **Windows `.zip` / Linux `.deb`**: only via CI - the cross-compile toolchains
  aren't set up locally.
- **iOS**: not built (very limited; document the gap when attempted).
- **Small edges**: pinned snippets show only in the running view; the clip-id
  dedup resets if the host restarts mid-session (restart Zulu to recover); image
  round-trips can produce one extra sync before settling (OS re-encoding).

## 8. Where things live (quick map)

| You want... | Look at |
| --- | --- |
| SSE push / presence primitive | `networking/crates/znet-core/src/web/events.rs` |
| Clip store + `/clip` + `/clips` | `networking/crates/znet-core/src/web/clips.rs` + `mod.rs` |
| Server, pairing, `index_html`, TLS wiring | `networking/crates/znet-core/src/web/mod.rs` |
| Self-signed cert + fingerprint | `networking/crates/znet-core/src/web/tls.rs` |
| Desktop shell (Host/Join, pins, secure, icon) | `networking/crates/zulu-desktop/src/main.rs` |
| Sync engine (receiver/sender, dedup) | `networking/crates/zulu-desktop/src/sync.rs` |
| TLS client (pinning) | `networking/crates/zulu-desktop/src/tlsclient.rs` |
| Image encode/decode | `networking/crates/zulu-desktop/src/imageclip.rs` |
| No-app web receiver | `networking/crates/zulu-desktop/src/zulu.html` |
| Android app (share target) | `networking/android/zulu/app/src/main/java/com/zulu/sync/` |
| Build / dist / CI | `scripts/build-dist.sh`, `.github/workflows/release.yml` |
| Original build brief | `docs/apps/zulu-brief.md` |
| Family vision + reuse rules | `docs/apps/README.md` |
