# zap - Backlog

> Working backlog. Detail/context lives in `docs/HANDOFF.md`. Ordered roughly
> Now → Next → Later → Ideas. Check items off as they land.

---

## Now - reliability (the paid pitch's backbone)

Reliability is what zap sells ("a cross-platform AirDrop that actually works").
Round 1 (done): host-side warns when no LAN IP + "same Wi-Fi / disable AP-client
isolation" hint.

**Round 2 - all done (2026-07-15):**

- [x] **`SO_REUSEADDR`** on the listener so quick restarts never hit `EADDRINUSE`
      (belt-and-suspenders over the single-acceptor clean-shutdown). Listener now
      built via `socket2` with `SO_REUSEADDR` before bind, handed to `tiny_http`
      via `from_listener`. Regression test in `zap-core`.
- [x] **AP / client-isolation detection** - host-side watchdog: while the server
      is reachable (has a LAN IP) but no client has connected (`requests_seen()==0`)
      after a 20s grace period, all three front-ends show a specific, actionable
      "No device has connected yet - same Wi-Fi? AP/client isolation? guest net?"
      message instead of a silent hang. (Client-side detection is impossible: under
      isolation the served page never loads at all.)
- [x] **Better `/api/list` failure message in the web UI** - distinguishes a
      never-reached host (connectivity → actionable "Can't reach this device"
      card with Wi-Fi / AP-isolation bullets + Try again) from a server-returned
      folder error (keeps "Could not open this folder").
- [x] Surface the host's reachability state - consistent green "Reachable at
      <url>" across CLI / desktop / Android, with the URL made prominent.

---

## Investigation - device pairing & faster transfer

**Decision recorded (see HANDOFF rationale):** pairing itself does **not** make
transfers faster - it's a **trust/convenience** feature. Real speed gains need a
**router-bypass** transport, which is platform-limited and works against zap's
"no app on the receiver" + "no discovery" differentiators.

- [ ] **Pairing (convenience, on-brand)** - when zap is native on both ends:
      remember trusted devices, skip QR/login on reconnect, auto-reconnect on the
      same LAN, show a "known devices" list. **Do not market as "faster."**
- [ ] **Cheap speed wins (no P2P):** parallel multi-file uploads; larger socket
      buffers; confirm keep-alive is optimal. Modest, cross-platform-safe.
- [ ] **Optional "Turbo / Direct mode" (later, advanced):** router bypass via
      Wi-Fi Direct / hotspot for **Android↔Android / Android↔Windows only**.
      - Excludes iOS/macOS (no third-party Wi-Fi Direct / AWDL interop).
      - Requires native app on both ends + reintroduces discovery/pairing.
      - Only worth it when the router is the bottleneck (weak/congested) or no
        shared network exists. Keep it opt-in, never the default.

---

## Next - Play Store (Android is the paid product)

- [ ] Release **signing** + build **AAB** (upload keystore; Play App Signing).
- [ ] `MANAGE_EXTERNAL_STORAGE` **declaration form** + demo video (approval risk;
      plan-B = SAF-only scoped storage if rejected).
- [ ] **Privacy policy** URL (host on GitHub Pages) - "files stay on your LAN,
      no data collected."
- [ ] **Data-safety** form (no data collected/shared) + content rating.
- [ ] **Closed testing** - 20 testers × 14 days (new personal-account rule).
- [ ] Store listing: icon 512, feature graphic 1024×500, screenshots, copy.

---

## Next - monetization

- [ ] Decide **paid-upfront (~₹50) vs free + Pro (IAP)**.
- [ ] If IAP: pick the Pro line (e.g. no size cap, turbo mode, themes) and wire
      Play Billing.
- [ ] Before public repo: **extract the paid Android app** out of the monorepo
      (git history is permanent) so `zap-core`/`cli`/`desktop` can open-source.

---

## Later - desktop distribution polish

- [x] **Universal macOS binary** (x86_64 + aarch64 via `lipo`) - done
      (2026-07-15). `build-dist.sh` builds both arches, lipos the CLI, swaps a
      fat binary into the `.app`, and repackages the `.dmg` via `hdiutil`.
- [ ] **Code signing / notarization** (macOS Apple Developer; Windows cert) to
      kill Gatekeeper / SmartScreen warnings.
- [ ] **Validate Windows/Linux installers** end-to-end (currently only produced
      by CI, not run/tested).

---

## Ideas / backlog (unscheduled)

- [ ] **iOS** app (not built).
- [ ] **Optional mDNS auto-discovery** as a *convenience layer on top of* the
      explicit-URL model (never a replacement - explicit URL stays the reliable
      default).
- [ ] **Transfer history** (persist past transfers).
- [ ] **Themes** / appearance options.
- [ ] Validate the pain beyond the owner's circle (soft launch / user interviews).

---

## Known limitations (accept or address)

- Needs both devices on the **same subnet** with **no AP/client isolation**
  (zap removes the *discovery* failure mode, not *all* network failures).
- Speed = normal Wi-Fi LAN throughput (no P2P today).
- macOS `.dmg` arm64-only; all desktop builds unsigned.
