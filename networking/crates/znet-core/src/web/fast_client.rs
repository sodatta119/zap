//! Native fast-lane **client**, with transparent HTTP fallback.
//!
//! Given a Zap download link, this probes `GET /api/capabilities`; if a fast
//! lane is advertised and reachable it downloads over the custom TCP protocol
//! (see [`super::fast`] and `docs/fast-lane-protocol.md`), otherwise - or on any
//! fast-lane failure - it finishes over the existing HTTP path, resuming by
//! offset from whatever bytes are already on disk. The HTTP path is always the
//! safety net, so a fast-lane problem never fails a transfer HTTP could complete.
//!
//! v1 speaks **plain HTTP only** on the client side; HTTPS/pinned-cert support
//! arrives together with fast-lane TLS in a later phase. The server advertises
//! the fast lane only when it is on plain HTTP (see `web::capabilities_json`), so
//! this stays consistent.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use super::{crc32_file, fast};

/// Streaming buffer size.
const CHUNK: usize = 128 * 1024;

/// The outcome of a completed [`get`].
#[derive(Debug, Clone)]
pub struct Report {
    /// Final path the file was written to.
    pub path: PathBuf,
    /// Whole-file size in bytes.
    pub total: u64,
    /// True if the fast lane carried the transfer; false if it fell back to HTTP.
    pub used_fast: bool,
    /// True if a whole-file CRC-32 was checked and matched (fast lane only).
    pub verified: bool,
    /// Bytes already on disk when the download started (0 for a fresh download,
    /// non-zero when it resumed a partial `.zap-part-` file).
    pub resumed_from: u64,
    /// Number of parallel connections the fast lane actually used (0 for the
    /// HTTP fallback).
    pub streams: usize,
}

/// A Zap download link, broken into its parts.
#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    /// The file path relative to the share root, still percent-encoded (reused
    /// verbatim when rebuilding an HTTP `/download?path=` request).
    raw_path: String,
    /// The same path, percent-decoded (what the fast handshake and `resolve_within`
    /// expect, and where the filename is derived from).
    file_path: String,
    /// The pairing/session token (`?k=`), if the link carried one.
    token: Option<String>,
    /// The leaf filename to save as.
    filename: String,
}

/// Tunables for the fast lane. `streams` is the number of parallel TCP
/// connections; `chunk_size` is the byte range each connection requests at a
/// time. When `adaptive` is set (the default), the download ramps concurrency up
/// to `streams` (used as a cap) and sizes chunks live from measured
/// throughput/RTT, starting from `chunk_size`; when it is clear, `streams` and
/// `chunk_size` are used as-is (fixed), which is handy for A/B experiments.
#[derive(Debug, Clone, Copy)]
pub struct GetOptions {
    pub streams: usize,
    pub chunk_size: u64,
    pub adaptive: bool,
}

/// Adaptive by default: ramp up to 8 connections, start at 4 MiB chunks.
impl Default for GetOptions {
    fn default() -> Self {
        GetOptions {
            streams: 8,
            chunk_size: 4 << 20, // 4 MiB
            adaptive: true,
        }
    }
}

/// Download the file named by a Zap link into `dest` (a file path or a
/// directory), using the fast lane when available and falling back to HTTP.
pub fn get(url: &str, dest: &Path) -> Result<Report> {
    get_with(url, dest, GetOptions::default())
}

/// Like [`get`], with explicit fast-lane tunables.
pub fn get_with(url: &str, dest: &Path, opts: GetOptions) -> Result<Report> {
    let target = parse_target(url)?;
    let (folder, final_path) = resolve_dest(dest, &target.filename);
    fs::create_dir_all(&folder)
        .with_context(|| format!("creating destination folder {}", folder.display()))?;
    let dest_name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&target.filename)
        .to_string();
    // Reuse the exact same temp-file naming as the HTTP resumable path so the two
    // transports share one checkpoint and could even resume each other.
    let part = super::part_path(&folder, &dest_name);

    // Discover the fast lane over HTTP. A failure here just means "no fast lane".
    let fast_port = probe_fast_port(&target).unwrap_or(None);

    if let Some(fp) = fast_port {
        match fast_download(&target, fp, &part, opts) {
            Ok((total, verified, resumed, streams)) => {
                finalize(&part, &final_path)?;
                return Ok(Report {
                    path: final_path,
                    total,
                    used_fast: true,
                    verified,
                    resumed_from: resumed,
                    streams,
                });
            }
            Err(e) => {
                eprintln!("zap: fast lane failed ({e:#}); falling back to HTTP");
            }
        }
    }

    // HTTP fallback - resumes from whatever the fast lane already wrote (a
    // multi-stream attempt leaves a valid contiguous prefix on failure).
    let (total, resumed) = http_download(&target, &part)?;
    finalize(&part, &final_path)?;
    Ok(Report {
        path: final_path,
        total,
        used_fast: false,
        verified: false,
        resumed_from: resumed,
        streams: 0,
    })
}

/// Atomically move the completed temp file into place. A partial file is never
/// exposed under the final name - the caller only reaches here after the size
/// (and, on the fast lane, the CRC) has been verified.
fn finalize(part: &Path, final_path: &Path) -> Result<()> {
    fs::rename(part, final_path)
        .with_context(|| format!("finalizing {}", final_path.display()))
}

// ---- Fast lane (multi-stream, adaptive) ----

/// Max times a single range may fail (across the whole pool) before the fast
/// lane gives up and the caller falls back to HTTP.
const MAX_RANGE_ATTEMPTS: u32 = 4;
/// Concurrency floor / starting point when adapting (the brief's "start ~4").
const MIN_STREAMS: usize = 4;
/// Chunk-size bounds - modest so a dropped stream re-fetches little (brief 3.5).
const MIN_CHUNK: u64 = 1 << 20; // 1 MiB
const MAX_CHUNK: u64 = 8 << 20; // 8 MiB
/// How often the controller samples throughput and re-tunes.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// A byte range `[offset, offset+len)` of the file - one unit of parallel work.
type Range = (u64, u64);

/// Dynamic work allocator: hands out ranges from a moving cursor at the current
/// (adaptive) chunk size, tracks completed ranges (for the resume prefix), and
/// holds failed ranges for retry. Guarded by one mutex in [`Shared`].
struct Alloc {
    start: u64,
    cursor: u64,
    total: u64,
    chunk_size: u64,
    reclaim: Vec<Range>,
    completed: Vec<Range>,
    attempts: HashMap<u64, u32>,
}

impl Alloc {
    /// Claim the next range: a failed range to retry if any, else a fresh
    /// `chunk_size` slice from the cursor. `None` when nothing is left to hand
    /// out (work may still be in flight in other workers).
    fn claim(&mut self) -> Option<Range> {
        if let Some(r) = self.reclaim.pop() {
            return Some(r);
        }
        if self.cursor < self.total {
            let len = self.chunk_size.min(self.total - self.cursor);
            let r = (self.cursor, len);
            self.cursor += len;
            return Some(r);
        }
        None
    }

    fn complete(&mut self, r: Range) {
        self.completed.push(r);
    }

    /// Record a failed range. Returns true if it has failed too many times (the
    /// caller should abort the fast lane); otherwise it is queued for retry.
    fn fail(&mut self, r: Range) -> bool {
        let n = self.attempts.entry(r.0).or_insert(0);
        *n += 1;
        if *n >= MAX_RANGE_ATTEMPTS {
            true
        } else {
            self.reclaim.push(r);
            false
        }
    }

    /// True once every byte has been handed out and completed.
    fn all_done(&self) -> bool {
        self.reclaim.is_empty() && self.cursor >= self.total && self.contiguous_prefix() >= self.total
    }

    /// True while there is still work to hand out (fresh or reclaimed).
    fn work_remaining(&self) -> bool {
        !self.reclaim.is_empty() || self.cursor < self.total
    }

    /// The length of the contiguous, gap-free prefix of completed ranges from
    /// `start` - the point the temp file is valid up to for resume/fallback.
    fn contiguous_prefix(&self) -> u64 {
        let mut ends: Vec<Range> = self.completed.clone();
        ends.sort_by_key(|(off, _)| *off);
        let mut prefix = self.start;
        for (off, len) in ends {
            if off <= prefix {
                prefix = prefix.max(off + len);
            } else {
                break;
            }
        }
        prefix
    }
}

/// State shared between the controller and the worker pool.
struct Shared {
    alloc: Mutex<Alloc>,
    part: PathBuf,
    /// Bytes successfully written (drives throughput measurement).
    bytes: AtomicU64,
    /// Cumulative failed ranges (drives the back-off signal).
    errors: AtomicU64,
    /// Workers currently running.
    active: AtomicUsize,
    /// Target worker count the controller is steering toward.
    desired: AtomicUsize,
    /// Peak concurrency actually reached (reported back).
    peak: AtomicUsize,
    /// EWMA of TCP connect time in nanoseconds (an RTT proxy); 0 = unknown.
    rtt_ns: AtomicU64,
    /// Set to unwind everything on a fatal error, or once complete.
    abort: AtomicBool,
    done: AtomicBool,
}

/// Drive an (optionally adaptive) multi-stream fast-lane download.
///
/// Stats the file once (learning `total` + whole-file CRC and warming the
/// server's CRC cache), pre-sizes the temp file, then runs an elastic pool of
/// worker connections that pull ranges from a shared allocator and write them at
/// their absolute offset (positioned writes, no hot-path locking). When adaptive,
/// a controller samples aggregate throughput every [`SAMPLE_INTERVAL`], hill-
/// climbs the connection count up to `opts.streams`, and sizes new ranges from
/// measured throughput/RTT - logging each decision. On success it verifies the
/// whole-file size + CRC; on unrecoverable failure it truncates the temp file to
/// its contiguous prefix (a valid resume point) and errors so the caller finishes
/// over HTTP. Returns `(total, verified, resumed_from, peak_streams)`.
fn fast_download(
    t: &Target,
    fast_port: u16,
    part: &Path,
    opts: GetOptions,
) -> Result<(u64, bool, u64, usize)> {
    let (total, crc) = fast_stat(t, fast_port)?;

    // The on-disk temp file is always a contiguous prefix (we truncate to one on
    // any failure), so its length is a valid resume offset.
    let mut start = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if start > total {
        start = 0;
    }

    // Pre-size to the full length so workers can write ranges at absolute offsets;
    // the existing [0, start) prefix is preserved, the rest zero-filled.
    {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(part)
            .with_context(|| format!("opening {}", part.display()))?;
        file.set_len(total)?;
    }

    if start == total {
        let verified = verify_crc(part, crc)?;
        if crc.is_some() && !verified {
            let _ = fs::remove_file(part);
            bail!("integrity check failed (CRC mismatch)");
        }
        return Ok((total, verified, start, 0));
    }

    let max_streams = opts.streams.max(1);
    let min_streams = if opts.adaptive {
        MIN_STREAMS.min(max_streams)
    } else {
        max_streams
    };
    let init_chunk = if opts.adaptive {
        opts.chunk_size.clamp(MIN_CHUNK, MAX_CHUNK)
    } else {
        opts.chunk_size.max(64 * 1024)
    };

    let shared = Arc::new(Shared {
        alloc: Mutex::new(Alloc {
            start,
            cursor: start,
            total,
            chunk_size: init_chunk,
            reclaim: Vec::new(),
            completed: Vec::new(),
            attempts: HashMap::new(),
        }),
        part: part.to_path_buf(),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        active: AtomicUsize::new(0),
        desired: AtomicUsize::new(min_streams),
        peak: AtomicUsize::new(0),
        rtt_ns: AtomicU64::new(0),
        abort: AtomicBool::new(false),
        done: AtomicBool::new(false),
    });

    run_pool(&shared, t, fast_port, opts, min_streams, max_streams);

    let all_done = lock(&shared.alloc).all_done();
    if !all_done {
        let prefix = lock(&shared.alloc).contiguous_prefix();
        if let Ok(file) = OpenOptions::new().write(true).open(part) {
            let _ = file.set_len(prefix);
        }
        bail!("fast lane incomplete after retries (contiguous prefix {prefix}/{total})");
    }

    let got = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if got != total {
        bail!("fast lane size mismatch: {got}/{total}");
    }
    let verified = verify_crc(part, crc)?;
    if crc.is_some() && !verified {
        let _ = fs::remove_file(part);
        bail!("integrity check failed (CRC mismatch)");
    }
    let peak = shared.peak.load(Ordering::SeqCst).max(1);
    Ok((total, verified, start, peak))
}

/// Lock a mutex, tolerating poisoning (a panicked worker must not wedge us).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run the controller loop: keep `desired` workers alive, and (when adaptive)
/// re-tune `desired` + chunk size from measured throughput/RTT every interval.
fn run_pool(
    shared: &Arc<Shared>,
    t: &Target,
    fast_port: u16,
    opts: GetOptions,
    min_streams: usize,
    max_streams: usize,
) {
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut prev_bytes = 0u64;
    let mut prev_tput = 0.0f64;
    let mut prev_errors = 0u64;

    loop {
        if shared.abort.load(Ordering::SeqCst) {
            break;
        }
        if lock(&shared.alloc).all_done() {
            break;
        }

        // Bring the pool up to `desired` while there is work to hand out.
        let desired = shared.desired.load(Ordering::SeqCst);
        let active = shared.active.load(Ordering::SeqCst);
        if lock(&shared.alloc).work_remaining() {
            for _ in active..desired {
                shared.active.fetch_add(1, Ordering::SeqCst);
                let peak_now = shared.active.load(Ordering::SeqCst);
                shared.peak.fetch_max(peak_now, Ordering::SeqCst);
                let shared = Arc::clone(shared);
                let t = t.clone();
                handles.push(thread::spawn(move || worker(shared, t, fast_port)));
            }
        }

        thread::sleep(SAMPLE_INTERVAL);

        // Measure the interval just elapsed.
        let now_bytes = shared.bytes.load(Ordering::SeqCst);
        let delta = now_bytes.saturating_sub(prev_bytes);
        prev_bytes = now_bytes;
        let tput = delta as f64 / SAMPLE_INTERVAL.as_secs_f64(); // bytes/s
        let now_errors = shared.errors.load(Ordering::SeqCst);
        let err_delta = now_errors.saturating_sub(prev_errors);
        prev_errors = now_errors;

        if opts.adaptive {
            let cur = shared.desired.load(Ordering::SeqCst);
            let next = decide_concurrency(prev_tput, tput, cur, min_streams, max_streams, err_delta);
            shared.desired.store(next, Ordering::SeqCst);

            let rtt_s = shared.rtt_ns.load(Ordering::SeqCst) as f64 / 1e9;
            let per_conn = if cur > 0 { tput / cur as f64 } else { tput };
            let cs = adaptive_chunk_size(per_conn, rtt_s, MIN_CHUNK, MAX_CHUNK);
            lock(&shared.alloc).chunk_size = cs;

            eprintln!(
                "zap: fast-lane adapt - streams {cur}->{next}, chunk {} KiB, ~{:.1} MB/s, rtt {:.1} ms, errs {err_delta}",
                cs / 1024,
                tput / 1_000_000.0,
                rtt_s * 1000.0,
            );
            prev_tput = tput;
        }
    }

    // Let idle/blocked workers exit and reap them.
    shared.done.store(true, Ordering::SeqCst);
    for h in handles {
        let _ = h.join();
    }
}

/// One worker: pull ranges and download them until retired, aborted, or done.
fn worker(shared: Arc<Shared>, t: Target, fast_port: u16) {
    // One write handle per worker; positioned writes need no cursor coordination.
    let file = match OpenOptions::new().write(true).open(&shared.part) {
        Ok(f) => f,
        Err(_) => {
            shared.abort.store(true, Ordering::SeqCst);
            shared.active.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    };
    loop {
        if shared.abort.load(Ordering::SeqCst) || shared.done.load(Ordering::SeqCst) {
            break;
        }
        // Retire if the controller has scaled concurrency down.
        if shared.active.load(Ordering::SeqCst) > shared.desired.load(Ordering::SeqCst) {
            break;
        }
        let claimed = lock(&shared.alloc).claim();
        let range = match claimed {
            Some(r) => r,
            None => {
                // Nothing to hand out now: stop if finished, else a reclaim may
                // still appear from another worker - wait briefly and recheck.
                if lock(&shared.alloc).all_done() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
        };
        match download_range(&shared, &t, fast_port, &file, range) {
            Ok(written) => {
                shared.bytes.fetch_add(written, Ordering::SeqCst);
                lock(&shared.alloc).complete(range);
            }
            Err(_) => {
                shared.errors.fetch_add(1, Ordering::SeqCst);
                let over = lock(&shared.alloc).fail(range);
                if over {
                    shared.abort.store(true, Ordering::SeqCst);
                    break;
                }
                thread::sleep(Duration::from_millis(150));
            }
        }
    }
    shared.active.fetch_sub(1, Ordering::SeqCst);
}

/// Steer the connection count from a throughput sample. While throughput keeps
/// improving and no ranges are failing, ramp up multiplicatively (slow-start:
/// double toward `max`) so a good link reaches useful concurrency in a couple of
/// samples; back off by one on errors or a throughput drop (AIMD-style), and hold
/// on a plateau. Clamped to `[min, max]`.
fn decide_concurrency(
    prev_tput: f64,
    cur_tput: f64,
    desired: usize,
    min: usize,
    max: usize,
    err_delta: u64,
) -> usize {
    if err_delta > 0 && desired > min {
        return (desired - 1).max(min); // link is dropping ranges: ease off
    }
    if cur_tput > prev_tput * 1.10 && desired < max {
        return (desired * 2).min(max); // still helping: grow fast
    }
    if cur_tput < prev_tput * 0.90 && desired > min {
        return desired - 1; // throughput fell: shed a stream
    }
    desired
}

/// Size the next range from measured per-connection throughput and RTT: big
/// enough to keep several RTTs in flight (fill the pipe), but capped so a dropped
/// stream re-fetches only ~0.5s of data. Clamped to `[min, max]`.
fn adaptive_chunk_size(per_conn_bps: f64, rtt_s: f64, min: u64, max: u64) -> u64 {
    if per_conn_bps <= 0.0 {
        return (4 << 20u64).clamp(min, max);
    }
    let by_time = per_conn_bps * 0.5; // ~0.5s of data
    let by_bdp = per_conn_bps * rtt_s * 4.0; // keep ~4 RTTs in flight
    let cs = by_time.max(by_bdp) as u64;
    cs.clamp(min, max)
}

/// Fold a fresh connect duration into the shared RTT estimate (EWMA, 1/8 weight).
fn record_rtt(shared: &Shared, dt: Duration) {
    let ns = dt.as_nanos().min(u64::MAX as u128) as u64;
    let old = shared.rtt_ns.load(Ordering::SeqCst);
    let new = if old == 0 { ns } else { (old * 7 + ns) / 8 };
    shared.rtt_ns.store(new, Ordering::SeqCst);
}

/// Download one range over its own connection (timing the connect as an RTT
/// proxy) and write the bytes at their absolute offset in `file`. Returns bytes
/// written, or an error on a short read so the caller can requeue the range.
fn download_range(shared: &Shared, t: &Target, fast_port: u16, file: &std::fs::File, r: Range) -> Result<u64> {
    let (offset, len) = r;

    let connect_start = Instant::now();
    let mut stream = connect_fast(t, fast_port)?;
    record_rtt(shared, connect_start.elapsed());

    write_handshake(&mut stream, t, offset, len)?;
    let _ = read_reply(&mut stream)?; // total/CRC already known from the stat

    let mut pos = offset;
    let mut remaining = len;
    let mut buf = [0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = stream.read(&mut buf[..want])?;
        if n == 0 {
            bail!("connection closed with {remaining} bytes left in range");
        }
        write_at(file, pos, &buf[..n])?;
        pos += n as u64;
        remaining -= n as u64;
    }
    Ok(len)
}

/// Verify the temp file's whole-file CRC-32 against `expected`. `Ok(true)` when
/// it matches (or there is nothing to check but the caller treats "no CRC" as
/// unverified via the `crc.is_some()` guard).
fn verify_crc(part: &Path, expected: Option<u32>) -> Result<bool> {
    match expected {
        Some(want) => {
            let got = crc32_file(part).map_err(|e| anyhow!("reading for CRC: {e}"))?;
            Ok(got == want)
        }
        None => Ok(false),
    }
}

/// Stat the file over the fast lane: a handshake with a zero-length range
/// (offset past EOF) returns `total_size` + optional whole-file CRC and no data.
/// This also warms the server's CRC cache before the parallel workers connect.
fn fast_stat(t: &Target, fast_port: u16) -> Result<(u64, Option<u32>)> {
    let mut stream = connect_fast(t, fast_port)?;
    write_handshake(&mut stream, t, u64::MAX, 0)?;
    let (total, crc) = read_reply(&mut stream)?;
    Ok((total, crc))
}

/// Open a fast-lane connection with sensible socket options.
fn connect_fast(t: &Target, fast_port: u16) -> Result<TcpStream> {
    let stream = TcpStream::connect((t.host.as_str(), fast_port))
        .with_context(|| format!("connecting to fast lane {}:{fast_port}", t.host))?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    Ok(stream)
}

/// Write a GET handshake for `[offset, offset+range_len)` (range_len 0 = to EOF).
fn write_handshake(stream: &mut TcpStream, t: &Target, offset: u64, range_len: u64) -> Result<()> {
    let token = t.token.clone().unwrap_or_default();
    let path_bytes = t.file_path.as_bytes();
    let mut hs = Vec::with_capacity(27 + token.len() + path_bytes.len());
    hs.extend_from_slice(fast::MAGIC);
    hs.extend_from_slice(&fast::VERSION.to_le_bytes());
    hs.push(fast::OP_GET);
    hs.extend_from_slice(&(token.len() as u16).to_le_bytes());
    hs.extend_from_slice(token.as_bytes());
    hs.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
    hs.extend_from_slice(path_bytes);
    hs.extend_from_slice(&offset.to_le_bytes());
    hs.extend_from_slice(&range_len.to_le_bytes());
    stream.write_all(&hs)?;
    stream.flush()?;
    Ok(())
}

/// Read a handshake reply: `Ok((total_size, optional_crc))`, or an error carrying
/// the server's message.
fn read_reply(stream: &mut TcpStream) -> Result<(u64, Option<u32>)> {
    let mut status = [0u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != fast::ST_OK {
        let mut lenb = [0u8; 2];
        stream.read_exact(&mut lenb)?;
        let mlen = u16::from_le_bytes(lenb) as usize;
        let mut msg = vec![0u8; mlen];
        stream.read_exact(&mut msg).ok();
        bail!("server said: {}", String::from_utf8_lossy(&msg));
    }
    let mut b8 = [0u8; 8];
    stream.read_exact(&mut b8)?;
    let total = u64::from_le_bytes(b8);
    let mut b1 = [0u8; 1];
    stream.read_exact(&mut b1)?;
    let crc = if b1[0] == 1 {
        let mut b4 = [0u8; 4];
        stream.read_exact(&mut b4)?;
        Some(u32::from_le_bytes(b4))
    } else {
        None
    };
    Ok((total, crc))
}

/// Write `buf` at absolute `offset` in `file`. On unix this is a lock-free
/// positioned write (`pwrite`), so parallel workers never contend on a cursor.
#[cfg(unix)]
fn write_at(file: &std::fs::File, offset: u64, buf: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)?;
    Ok(())
}

/// Portable fallback for non-unix targets: serialize positioned writes through a
/// global lock (Zap's real targets are all unix, so this path is a compile-time
/// safety net, not the hot path).
#[cfg(not(unix))]
fn write_at(file: &std::fs::File, offset: u64, buf: &[u8]) -> Result<()> {
    use std::sync::Mutex as StdMutex;
    static WRITE_LOCK: StdMutex<()> = StdMutex::new(());
    let _g = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(buf)?;
    Ok(())
}

// ---- HTTP fallback ----

/// Finish (or perform) the download over the HTTP path, resuming from any bytes
/// already on disk via a `Range` request. Returns the whole-file size and the
/// offset it resumed from.
fn http_download(t: &Target, part: &Path) -> Result<(u64, u64)> {
    let on_disk = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut headers = auth_headers(t);
    if on_disk > 0 {
        headers.push(("Range".to_string(), format!("bytes={on_disk}-")));
    }
    let target = format!("/download?path={}", t.raw_path);
    let mut resp = http_request(&t.host, t.port, "GET", &target, &headers)?;
    if resp.status != 200 && resp.status != 206 {
        bail!("HTTP download failed: status {}", resp.status);
    }
    let body_len = resp
        .content_length
        .ok_or_else(|| anyhow!("HTTP response had no Content-Length"))?;

    // 206 => the server honored our Range and resumed; 200 => it sent the whole
    // file (Range ignored or none requested), so restart from zero.
    let start = if resp.status == 206 { on_disk } else { 0 };
    let total = if resp.status == 206 { start + body_len } else { body_len };

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(part)
        .with_context(|| format!("opening {}", part.display()))?;
    file.seek(SeekFrom::Start(start))?;
    file.set_len(start)?;

    let mut remaining = body_len;
    let mut buf = [0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = resp.reader.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    file.flush()?;

    let got = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if got != total {
        bail!("HTTP download incomplete: {got}/{total} bytes");
    }
    Ok((total, start))
}

/// Probe `GET /api/capabilities` and extract the advertised fast-lane port, if
/// any. Returns `Ok(None)` when there is no fast lane (older server, HTTPS, or
/// the field is `null`).
fn probe_fast_port(t: &Target) -> Result<Option<u16>> {
    let headers = auth_headers(t);
    let mut resp = http_request(&t.host, t.port, "GET", "/api/capabilities", &headers)?;
    if resp.status != 200 {
        return Ok(None);
    }
    let mut body = String::new();
    resp.reader.read_to_string(&mut body).ok();
    Ok(parse_fast_port(&body))
}

/// Extract `fast.port` from the capabilities JSON. Deliberately tiny and
/// dependency-free (the crate hand-writes JSON everywhere else too).
fn parse_fast_port(json: &str) -> Option<u16> {
    let compact: String = json.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("\"fast\":null") {
        return None;
    }
    let idx = compact.find("\"port\":")?;
    let after = &compact[idx + "\"port\":".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The auth header a native client sends: the pairing token as the session
/// cookie the HTTP server already understands (`has_valid_session`).
fn auth_headers(t: &Target) -> Vec<(String, String)> {
    match &t.token {
        Some(tok) => vec![("Cookie".to_string(), format!("zap_session={tok}"))],
        None => Vec::new(),
    }
}

/// A minimal HTTP/1.1 response: status, headers, a reader positioned at the body
/// start, and the parsed `Content-Length`.
struct HttpResponse {
    status: u16,
    #[allow(dead_code)]
    headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    content_length: Option<u64>,
}

/// Perform a minimal plain-HTTP/1.1 request (`Connection: close`, one request per
/// connection) and return the response with its body reader ready.
fn http_request(
    host: &str,
    port: u16,
    method: &str,
    target: &str,
    extra: &[(String, String)],
) -> Result<HttpResponse> {
    let stream = TcpStream::connect((host, port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();

    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    {
        let mut w = &stream;
        w.write_all(req.as_bytes())?;
        w.flush()?;
    }

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = parse_status(&status_line)?;

    let mut headers = Vec::new();
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().ok();
            }
            headers.push((k, v));
        }
    }
    Ok(HttpResponse {
        status,
        headers,
        reader,
        content_length,
    })
}

fn parse_status(line: &str) -> Result<u16> {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {line:?}"))
}

// ---- URL + destination parsing ----

/// Parse a Zap download link. Must be `http://host[:port]/...?path=<file>` with
/// an optional `&k=<token>`. HTTPS is rejected in v1 (plain HTTP only).
fn parse_target(url: &str) -> Result<Target> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        if url.starts_with("https://") {
            anyhow!("`zap get` speaks plain HTTP only for now; open this https link in a browser instead")
        } else {
            anyhow!("expected an http:// URL, got: {url}")
        }
    })?;

    let (authority, pathq) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_authority(authority);

    let (_, query) = super::split_query(pathq);
    let raw_path = super::query_param(query, "path")
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("the URL must include ?path=<file> (a Zap download link)"))?;
    let file_path = super::decode_percent(&raw_path);
    let token = super::query_param(query, "k").map(super::decode_percent);

    let filename = file_path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("download")
        .to_string();
    if filename.is_empty() {
        bail!("could not determine a filename from the URL");
    }

    Ok(Target {
        host,
        port,
        raw_path,
        file_path,
        token,
        filename,
    })
}

/// Split `host`, `host:port`, or `[ipv6]:port` into (host, port), defaulting to
/// port 80.
fn split_authority(authority: &str) -> (String, u16) {
    if let Some(after) = authority.strip_prefix('[') {
        // [ipv6]:port
        let (h, tail) = after.split_once(']').unwrap_or((after, ""));
        let port = tail.trim_start_matches(':').parse().unwrap_or(80);
        (h.to_string(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        (h.to_string(), p.parse().unwrap_or(80))
    } else {
        (authority.to_string(), 80)
    }
}

/// Resolve the destination folder and final path. If `dest` is an existing
/// directory (or empty), the file lands inside it under its own name; otherwise
/// `dest` is treated as the target file path.
fn resolve_dest(dest: &Path, filename: &str) -> (PathBuf, PathBuf) {
    if dest.as_os_str().is_empty() || dest.is_dir() {
        let folder = if dest.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dest.to_path_buf()
        };
        let final_path = folder.join(filename);
        (folder, final_path)
    } else {
        let folder = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        (folder, dest.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr, TcpListener};
    use std::sync::Mutex;

    use crate::web::{spawn, ServeConfig};

    /// Serialize the tests that stand up a real server (they pick an ephemeral
    /// port and rebind it), so two do not race on the same freed port.
    static GUARD: Mutex<()> = Mutex::new(());

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn parse_target_reads_all_parts() {
        let t = parse_target("http://192.168.1.5:8080/download?path=sub%2Fmovie.mp4&k=abc123")
            .expect("parse");
        assert_eq!(t.host, "192.168.1.5");
        assert_eq!(t.port, 8080);
        assert_eq!(t.raw_path, "sub%2Fmovie.mp4", "raw path kept encoded for HTTP reuse");
        assert_eq!(t.file_path, "sub/movie.mp4", "decoded path for the handshake");
        assert_eq!(t.token.as_deref(), Some("abc123"));
        assert_eq!(t.filename, "movie.mp4");
    }

    #[test]
    fn parse_target_defaults_port_and_rejects_bad_input() {
        let t = parse_target("http://host/download?path=x.txt").expect("default port");
        assert_eq!(t.port, 80);
        assert!(t.token.is_none());
        assert!(parse_target("https://host/download?path=x.txt").is_err(), "https rejected in v1");
        assert!(parse_target("http://host/").is_err(), "missing ?path rejected");
    }

    #[test]
    fn parse_fast_port_handles_null_and_value() {
        assert_eq!(parse_fast_port("{\"fast\":null}"), None);
        assert_eq!(
            parse_fast_port("{\"fast\":{\"port\":50370,\"tls\":false,\"version\":1}}"),
            Some(50370)
        );
        // Whitespace-tolerant.
        assert_eq!(parse_fast_port("{ \"fast\": { \"port\": 41000 } }"), Some(41000));
    }

    /// The HTTP fallback transport must download byte-exact and resume from a
    /// partial temp file via a Range request - the safety net when the fast lane
    /// is unavailable.
    #[test]
    fn http_fallback_downloads_and_resumes() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let src = std::env::temp_dir().join(format!("zap-fbk-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("zap-fbk-dst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let data: Vec<u8> = (0..250_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ (i as u8))
            .collect();
        fs::write(src.join("f.bin"), &data).unwrap();

        let port = free_port();
        let (_info, handle) = spawn(ServeConfig {
            dir: src.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: None,
            index_html: None,
            tls: None,
        })
        .expect("bind");

        let target = Target {
            host: "127.0.0.1".to_string(),
            port,
            raw_path: "f.bin".to_string(),
            file_path: "f.bin".to_string(),
            token: None,
            filename: "f.bin".to_string(),
        };
        let part = dst.join(".zap-part-f.bin");

        // Fresh download over HTTP.
        let (total, resumed) = http_download(&target, &part).expect("http download");
        assert_eq!(total, data.len() as u64);
        assert_eq!(resumed, 0);
        assert_eq!(fs::read(&part).unwrap(), data, "HTTP download byte-exact");

        // Pre-seed a partial and confirm it resumes via Range (206).
        fs::write(&part, &data[..80_000]).unwrap();
        let (total2, resumed2) = http_download(&target, &part).expect("http resume");
        assert_eq!(total2, data.len() as u64);
        assert_eq!(resumed2, 80_000, "should resume from the on-disk offset");
        assert_eq!(fs::read(&part).unwrap(), data, "HTTP resume byte-exact");

        handle.stop();
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    fn new_alloc(start: u64, total: u64, chunk_size: u64) -> Alloc {
        Alloc {
            start,
            cursor: start,
            total,
            chunk_size,
            reclaim: Vec::new(),
            completed: Vec::new(),
            attempts: HashMap::new(),
        }
    }

    #[test]
    fn alloc_hands_out_chunks_and_a_ragged_tail() {
        let mut a = new_alloc(100, 1000, 300);
        assert_eq!(a.claim(), Some((100, 300)));
        assert_eq!(a.claim(), Some((400, 300)));
        assert_eq!(a.claim(), Some((700, 300)));
        // Only 900 bytes remained, so the last claim is the ragged tail... which
        // here is exactly consumed; nothing left.
        assert_eq!(a.claim(), None);

        // Ragged tail shorter than a full chunk.
        let mut b = new_alloc(0, 1000, 400);
        assert_eq!(b.claim(), Some((0, 400)));
        assert_eq!(b.claim(), Some((400, 400)));
        assert_eq!(b.claim(), Some((800, 200)));
        assert_eq!(b.claim(), None);
    }

    #[test]
    fn alloc_reclaims_failed_ranges_before_new_ones() {
        let mut a = new_alloc(0, 1000, 250);
        let r0 = a.claim().unwrap(); // (0,250)
        let _r1 = a.claim().unwrap(); // (250,250)
        assert!(!a.fail(r0), "first failure just requeues");
        // The reclaimed range is handed out ahead of fresh cursor work.
        assert_eq!(a.claim(), Some((0, 250)));
    }

    #[test]
    fn alloc_fail_aborts_after_the_attempt_budget() {
        let mut a = new_alloc(0, 100, 100);
        let r = a.claim().unwrap();
        // MAX_RANGE_ATTEMPTS failures -> the last returns true (abort).
        let mut over = false;
        for _ in 0..MAX_RANGE_ATTEMPTS {
            over = a.fail(r);
        }
        assert!(over, "should signal abort once the budget is exhausted");
    }

    #[test]
    fn alloc_contiguous_prefix_stops_at_first_gap() {
        let mut a = new_alloc(0, 1000, 250);
        // Complete [0,250), [250,500), [750,1000) - gap at [500,750).
        a.complete((0, 250));
        a.complete((250, 250));
        a.complete((750, 250));
        assert_eq!(a.contiguous_prefix(), 500);
        assert!(!a.all_done());
        // Fill the gap -> whole file is contiguous.
        a.complete((500, 250));
        // cursor hasn't reached total in this hand-built alloc, so all_done needs
        // the cursor advanced too; contiguous_prefix alone is the resume point.
        assert_eq!(a.contiguous_prefix(), 1000);
    }

    #[test]
    fn decide_concurrency_ramps_and_backs_off() {
        // Improving throughput -> grow multiplicatively (slow-start), capped.
        assert_eq!(decide_concurrency(100.0, 130.0, 3, 2, 8, 0), 6);
        assert_eq!(decide_concurrency(100.0, 130.0, 5, 2, 8, 0), 8, "doubling clamps to max");
        // Errors this interval -> shed a stream (down to min).
        assert_eq!(decide_concurrency(100.0, 130.0, 3, 2, 8, 2), 2);
        // Throughput dropped -> shed a stream.
        assert_eq!(decide_concurrency(100.0, 80.0, 4, 2, 8, 0), 3);
        // Plateau -> hold.
        assert_eq!(decide_concurrency(100.0, 103.0, 4, 2, 8, 0), 4);
        // At the cap, improving cannot push past max.
        assert_eq!(decide_concurrency(100.0, 200.0, 8, 2, 8, 0), 8);
        // At the floor, errors cannot push below min.
        assert_eq!(decide_concurrency(100.0, 50.0, 2, 2, 8, 5), 2);
    }

    #[test]
    fn adaptive_chunk_size_scales_and_clamps() {
        let min = 1 << 20;
        let max = 8 << 20;
        // Tiny throughput -> clamp to the floor.
        assert_eq!(adaptive_chunk_size(1000.0, 0.001, min, max), min);
        // Huge throughput -> clamp to the ceiling.
        assert_eq!(adaptive_chunk_size(1_000_000_000.0, 0.01, min, max), max);
        // Mid throughput (10 MB/s) -> ~0.5s of data = 5 MiB, within bounds.
        let cs = adaptive_chunk_size(10.0 * 1_048_576.0, 0.005, min, max);
        assert!(cs > min && cs < max, "expected an in-range chunk, got {cs}");
        // No measurement yet -> a sane default within bounds.
        let d = adaptive_chunk_size(0.0, 0.0, min, max);
        assert!(d >= min && d <= max);
    }

    /// Spin up a server sharing `src`, returning (port, handle).
    fn serve(src: &Path) -> (u16, crate::web::ServerHandle) {
        let port = free_port();
        let (_info, handle) = spawn(ServeConfig {
            dir: src.to_path_buf(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: None,
            index_html: None,
            tls: None,
        })
        .expect("bind");
        (port, handle)
    }

    /// Multi-stream download (many small chunks over several connections) must
    /// reassemble byte-exact and verify the whole-file CRC.
    #[test]
    fn multi_stream_download_byte_exact() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let src = std::env::temp_dir().join(format!("zap-ms-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("zap-ms-dst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let data: Vec<u8> = (0..1_000_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8 ^ (i as u8))
            .collect();
        fs::write(src.join("big.bin"), &data).unwrap();

        let (port, handle) = serve(&src);
        let url = format!("http://127.0.0.1:{port}/download?path=big.bin");
        // Fixed 6 streams, 64 KiB chunks -> ~16 ranges spread across connections.
        let opts = GetOptions { streams: 6, chunk_size: 64 * 1024, adaptive: false };
        let report = get_with(&url, &dst, opts).expect("multi-stream get");

        assert!(report.used_fast, "should use the fast lane");
        assert!(report.verified, "whole-file CRC should verify");
        assert_eq!(report.total, data.len() as u64);
        assert_eq!(report.streams, 6, "fixed mode uses exactly the requested streams");
        assert_eq!(fs::read(dst.join("big.bin")).unwrap(), data, "reassembled byte-exact");
        assert!(!dst.join(".zap-part-big.bin").exists(), "temp file removed");

        handle.stop();
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    /// Multi-stream resume: a contiguous partial prefix on disk is kept and only
    /// the remaining chunks are fetched, still byte-exact.
    #[test]
    fn multi_stream_resumes_from_partial() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let src = std::env::temp_dir().join(format!("zap-msr-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("zap-msr-dst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let data: Vec<u8> = (0..600_000u32)
            .map(|i| (i.wrapping_mul(40_503) >> 7) as u8 ^ (i as u8))
            .collect();
        fs::write(src.join("big.bin"), &data).unwrap();

        let seeded = 150_000usize;
        fs::write(dst.join(".zap-part-big.bin"), &data[..seeded]).unwrap();

        let (port, handle) = serve(&src);
        let url = format!("http://127.0.0.1:{port}/download?path=big.bin");
        let opts = GetOptions { streams: 4, chunk_size: 64 * 1024, adaptive: false };
        let report = get_with(&url, &dst, opts).expect("multi-stream resume");

        assert!(report.used_fast);
        assert_eq!(report.resumed_from, seeded as u64, "kept the on-disk prefix");
        assert_eq!(fs::read(dst.join("big.bin")).unwrap(), data, "resumed byte-exact");

        handle.stop();
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    /// Adaptive mode (the default) must still be correct: byte-exact + CRC
    /// verified, and it must resume a partial prefix. Concurrency/chunk sizing
    /// vary with the run, so we only assert correctness here; the tuning policy
    /// itself is unit-tested in `decide_concurrency`/`adaptive_chunk_size`.
    #[test]
    fn adaptive_download_and_resume_byte_exact() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let src = std::env::temp_dir().join(format!("zap-ad-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("zap-ad-dst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let data: Vec<u8> = (0..800_000u32)
            .map(|i| (i.wrapping_mul(2_246_822_519) >> 9) as u8 ^ (i as u8))
            .collect();
        fs::write(src.join("big.bin"), &data).unwrap();

        let (port, handle) = serve(&src);
        let url = format!("http://127.0.0.1:{port}/download?path=big.bin");

        // Fresh adaptive download.
        let opts = GetOptions { streams: 8, chunk_size: 256 * 1024, adaptive: true };
        let report = get_with(&url, &dst, opts).expect("adaptive get");
        assert!(report.used_fast && report.verified);
        assert!(report.streams >= 1, "peak concurrency reported");
        assert_eq!(fs::read(dst.join("big.bin")).unwrap(), data, "adaptive byte-exact");

        // Adaptive resume from a seeded partial.
        let _ = fs::remove_file(dst.join("big.bin"));
        fs::write(dst.join(".zap-part-big.bin"), &data[..123_456]).unwrap();
        let report2 = get_with(&url, &dst, opts).expect("adaptive resume");
        assert_eq!(report2.resumed_from, 123_456, "adaptive resumes from the prefix");
        assert_eq!(fs::read(dst.join("big.bin")).unwrap(), data, "adaptive resume byte-exact");

        handle.stop();
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }
}
