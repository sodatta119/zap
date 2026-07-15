//! Web transport (server mode).
//!
//! Unlike the host-driven [`Transport`](crate::transport::Transport) trait,
//! the web transport inverts control: the Mac runs an HTTP server on the LAN
//! and the phone drives transfers from its browser. No app is needed on the
//! phone.
//!
//! Endpoints:
//!   GET  /                    -> the web UI
//!   GET  /api/files           -> JSON list of shareable files
//!   GET  /download/<name>     -> download a shared file (Mac -> phone)
//!   PUT  /upload?name=<name>  -> upload a file (phone -> Mac); body is raw bytes

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const INDEX_HTML: &str = include_str!("index.html");
const LOGIN_HTML: &str = include_str!("login.html");

/// A username/password pair for HTTP Basic authentication.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub user: String,
    pub pass: String,
}

/// Configuration for a web-serve session.
pub struct ServeConfig {
    /// Directory whose files are offered for download and where uploads land.
    pub dir: PathBuf,
    /// Port to listen on.
    pub port: u16,
    /// Address to bind (defaults to all interfaces so other devices can reach it).
    pub bind: IpAddr,
    /// If set, every request must authenticate with these credentials.
    pub auth: Option<Credentials>,
}

/// Details about a running server, handed to the caller once it is bound and
/// listening. Callers (the CLI, an Android UI, …) use this to tell the user how
/// to connect — this crate itself performs no presentation.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Canonicalized directory being shared.
    pub dir: PathBuf,
    /// Port the server is listening on.
    pub port: u16,
    /// Best-guess LAN IP address of this host, if one could be determined.
    pub lan_ip: Option<IpAddr>,
}

impl ServerInfo {
    /// The URL another device on the same network should open. Falls back to
    /// `localhost` when no LAN IP could be determined.
    pub fn url(&self) -> String {
        let host = self
            .lan_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "localhost".to_string());
        format!("http://{host}:{}/", self.port)
    }
}

/// A running web server that can be stopped from another thread. Returned by
/// [`spawn`] for embedders (such as an Android foreground service) that start
/// the server and later need to shut it down. Dropping the handle also stops
/// the server and releases the port.
pub struct ServerHandle {
    server: Arc<Server>,
    acceptor: Option<thread::JoinHandle<()>>,
    stats: Arc<Stats>,
}

/// Running totals for a server, shared across worker threads.
#[derive(Default)]
pub struct Stats {
    bytes: AtomicU64,
}

impl ServerHandle {
    /// Total bytes transferred (uploads + downloads) since the server started.
    /// Poll this over time to compute live throughput.
    pub fn bytes_transferred(&self) -> u64 {
        self.stats.bytes.load(Ordering::Relaxed)
    }

    /// Stop the server and wait until the listening socket is released, so the
    /// port is free to bind again immediately (e.g. an app restart).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // `unblock` wakes exactly one thread blocked in `incoming_requests`,
        // and there is exactly one — the acceptor — so a single call is enough.
        self.server.unblock();
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        // The acceptor has now dropped its `Arc<Server>`. Once this handle's own
        // `Arc` drops too, `Server`'s destructor closes the listening socket.
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind the web server, invoke `on_ready` exactly once with the live
/// [`ServerInfo`], then serve requests until the process is stopped (blocking).
///
/// `on_ready` is where the caller presents connection details — the core does
/// no printing of its own, so the same server can back a terminal or a GUI.
pub fn serve(config: ServeConfig, on_ready: impl FnOnce(&ServerInfo)) -> Result<()> {
    let (server, dir, info) = bind(&config)?;
    on_ready(&info);
    let auth = Arc::new(build_auth(config.auth.as_ref()));
    let stats = Arc::new(Stats::default());
    // Use the calling thread as the acceptor; blocks until the process exits.
    accept_loop(&server, &dir, &auth, &stats);
    Ok(())
}

/// Bind and start the web server, returning immediately with the connection
/// details and a [`ServerHandle`] to stop it later. A single acceptor thread
/// runs in the background. This is the entry point embedders (e.g. the Android
/// app via JNI) use, since they can't block the calling thread.
pub fn spawn(config: ServeConfig) -> Result<(ServerInfo, ServerHandle)> {
    let (server, dir, info) = bind(&config)?;
    let auth = Arc::new(build_auth(config.auth.as_ref()));
    let stats = Arc::new(Stats::default());
    let acceptor = {
        let server = Arc::clone(&server);
        let dir = Arc::clone(&dir);
        let auth = Arc::clone(&auth);
        let stats = Arc::clone(&stats);
        thread::spawn(move || accept_loop(&server, &dir, &auth, &stats))
    };
    Ok((
        info,
        ServerHandle {
            server,
            acceptor: Some(acceptor),
            stats,
        },
    ))
}

/// Create the share directory, bind the listener, and assemble [`ServerInfo`].
fn bind(config: &ServeConfig) -> Result<(Arc<Server>, Arc<PathBuf>, ServerInfo)> {
    fs::create_dir_all(&config.dir)
        .with_context(|| format!("creating share directory {}", config.dir.display()))?;
    let dir = config
        .dir
        .canonicalize()
        .with_context(|| format!("resolving share directory {}", config.dir.display()))?;

    let addr = SocketAddr::new(config.bind, config.port);
    let server = Server::http(addr)
        .map_err(|e| anyhow::anyhow!("failed to start server on {addr}: {e}"))?;

    let dir = Arc::new(dir);
    let info = ServerInfo {
        dir: (*dir).clone(),
        port: config.port,
        lan_ip: lan_ip(),
    };
    Ok((Arc::new(server), dir, info))
}

/// Accept requests until the server is unblocked, handling each on its own
/// thread so that transfers run concurrently. Only this loop blocks on the
/// server, which is what makes a single `unblock()` a clean shutdown.
fn accept_loop(
    server: &Arc<Server>,
    dir: &Arc<PathBuf>,
    auth: &Arc<Option<Auth>>,
    stats: &Arc<Stats>,
) {
    for request in server.incoming_requests() {
        let dir = Arc::clone(dir);
        let auth = Arc::clone(auth);
        let stats = Arc::clone(stats);
        thread::spawn(move || {
            if let Err(e) = handle(request, &dir, (*auth).as_ref(), &stats) {
                eprintln!("zap: request error: {e:#}");
            }
        });
    }
}

fn handle(request: Request, dir: &Path, auth: Option<&Auth>, stats: &Stats) -> Result<()> {
    let method = request.method().clone();
    let raw_url = request.url().to_string();
    let (path, query) = split_query(&raw_url);

    // Session gate: serve a custom login page (no browser Basic-auth popup).
    if let Some(a) = auth {
        if method == Method::Post && path == "/login" {
            return handle_login(request, a);
        }
        if !has_valid_session(&request, a) {
            return match (&method, path.as_str()) {
                (Method::Get, "/") | (Method::Get, "/login") => {
                    respond(request, html_response(LOGIN_HTML))
                }
                _ => respond(
                    request,
                    Response::from_string("Unauthorized").with_status_code(401),
                ),
            };
        }
    }

    match (&method, path.as_str()) {
        (Method::Get, "/") => respond(request, html_response(INDEX_HTML)),
        (Method::Get, "/api/list") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            respond(request, json_response(&list_dir_json(dir, &rel)))
        }
        (Method::Get, "/api/search") => {
            let q = query_param(query, "q").map(decode_percent).unwrap_or_default();
            respond(request, json_response(&search_json(dir, &q)))
        }
        (Method::Get, "/download") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            serve_download(request, dir, &rel, stats)
        }
        (Method::Put, "/upload") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            let name = query_param(query, "name").map(decode_percent);
            handle_upload(request, dir, &rel, name.as_deref(), stats)
        }
        _ => respond(request, Response::from_string("Not found").with_status_code(404)),
    }
}

fn serve_download(request: Request, root: &Path, rel: &str, stats: &Stats) -> Result<()> {
    let Some(path) = resolve_within(root, rel) else {
        return respond(request, Response::from_string("Bad path").with_status_code(400));
    };
    let meta = match fs::metadata(&path) {
        Ok(m) if m.is_file() => m,
        _ => return respond(request, Response::from_string("Not found").with_status_code(404)),
    };
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("zap: opening {} failed: {e:#}", path.display());
            return respond(request, Response::from_string("read error").with_status_code(500));
        }
    };
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().replace('"', ""))
        .unwrap_or_else(|| "download".to_string());
    let disposition = format!("attachment; filename=\"{filename}\"");
    // Stream through a counting reader so throughput is tracked as bytes flow out.
    let reader = CountingReader { inner: file, stats };
    let headers = vec![
        header("Content-Type", "application/octet-stream"),
        header("Content-Disposition", &disposition),
    ];
    let response = Response::new(StatusCode(200), headers, reader, Some(meta.len() as usize), None);
    respond(request, response)
}

fn handle_upload(
    mut request: Request,
    root: &Path,
    rel_dir: &str,
    name: Option<&str>,
    stats: &Stats,
) -> Result<()> {
    let Some(name) = name.filter(|n| is_plain_filename(n)) else {
        return respond(request, Response::from_string("Bad or missing name").with_status_code(400));
    };
    let Some(folder) = resolve_within(root, rel_dir) else {
        return respond(request, Response::from_string("Bad path").with_status_code(400));
    };
    let dest = folder.join(name);

    // On failure we must still send a response, otherwise the client hangs and
    // reports a confusing generic error instead of a clean failure.
    match write_upload(&mut request, &dest, stats) {
        Ok(bytes) => {
            println!("received {name} ({bytes} bytes) into {}", folder.display());
            respond(request, Response::from_string("ok"))
        }
        Err(e) => {
            eprintln!("zap: upload of {name} failed: {e:#}");
            respond(request, Response::from_string("upload failed").with_status_code(500))
        }
    }
}

/// Stream the request body to `dest`, returning the number of bytes written.
fn write_upload(request: &mut Request, dest: &Path, stats: &Stats) -> Result<u64> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut writer = CountingWriter { inner: file, stats };
    io::copy(request.as_reader(), &mut writer).with_context(|| format!("writing {}", dest.display()))
}

/// A reader that adds every byte read to the shared throughput counter.
struct CountingReader<'a, R> {
    inner: R,
    stats: &'a Stats,
}
impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// A writer that adds every byte written to the shared throughput counter.
struct CountingWriter<'a, W> {
    inner: W,
    stats: &'a Stats,
}
impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---- Directory listing ----

/// List the directory at `root`/`rel` as JSON:
/// `{"path":"<normalized rel>","entries":[{"name","dir","size"?}...]}`.
/// Folders sort before files; both alphabetically. Returns an `error` object
/// if the path escapes the root or can't be read.
fn list_dir_json(root: &Path, rel: &str) -> String {
    let Some(dir) = resolve_within(root, rel) else {
        return r#"{"error":"bad path"}"#.to_string();
    };

    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_dir() {
                dirs.push((
                    name.to_lowercase(),
                    format!("{{\"name\":{},\"dir\":true}}", json_string(&name)),
                ));
            } else if meta.is_file() {
                files.push((
                    name.to_lowercase(),
                    format!(
                        "{{\"name\":{},\"dir\":false,\"size\":{}}}",
                        json_string(&name),
                        meta.len()
                    ),
                ));
            }
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let entries: Vec<String> = dirs
        .into_iter()
        .chain(files)
        .map(|(_, json)| json)
        .collect();

    format!(
        "{{\"path\":{},\"entries\":[{}]}}",
        json_string(&normalize_rel(rel)),
        entries.join(",")
    )
}

/// Collapse a relative path to clean `a/b/c` form (no empty or `.` segments).
fn normalize_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Result and scan caps for recursive search — keeps a huge tree from stalling.
const SEARCH_MAX_RESULTS: usize = 300;
const SEARCH_MAX_SCANNED: usize = 30_000;

/// Recursively search `root` for entries whose name contains `query`
/// (case-insensitive). Returns `{"query":..,"entries":[{path,name,dir,size?}..]}`.
fn search_json(root: &Path, query: &str) -> String {
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<String> = Vec::new();
    if !needle.is_empty() {
        let mut budget = SEARCH_MAX_SCANNED;
        search_walk(root, root, &needle, &mut hits, &mut budget);
    }
    format!(
        "{{\"query\":{},\"entries\":[{}]}}",
        json_string(query),
        hits.join(",")
    )
}

fn search_walk(root: &Path, dir: &Path, needle: &str, hits: &mut Vec<String>, budget: &mut usize) {
    if *budget == 0 || hits.len() >= SEARCH_MAX_RESULTS {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *budget == 0 || hits.len() >= SEARCH_MAX_RESULTS {
            return;
        }
        *budget -= 1;
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();

        if name.to_lowercase().contains(needle) {
            let rel = path
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            hits.push(if meta.is_dir() {
                format!("{{\"path\":{},\"name\":{},\"dir\":true}}", json_string(&rel), json_string(&name))
            } else {
                format!(
                    "{{\"path\":{},\"name\":{},\"dir\":false,\"size\":{}}}",
                    json_string(&rel),
                    json_string(&name),
                    meta.len()
                )
            });
        }
        if meta.is_dir() {
            search_walk(root, &path, needle, hits, budget);
        }
    }
}

// ---- Response helpers ----

fn respond<R: io::Read>(request: Request, response: Response<R>) -> Result<()> {
    request.respond(response).ok();
    Ok(())
}

fn html_response(body: &str) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(header("Content-Type", "text/html; charset=utf-8"))
}

fn json_response(body: &str) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(header("Content-Type", "application/json"))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header name/value are valid")
}

// ---- Authentication (custom login page + session cookie) ----

/// Runtime auth state: the expected credentials plus an unguessable session
/// token minted at startup. A client only receives the token (as a cookie)
/// after posting the correct credentials to `/login`.
struct Auth {
    user: String,
    pass: String,
    token: String,
}

const SESSION_COOKIE: &str = "zap_session";

/// Build the auth state, generating a fresh session token per run.
fn build_auth(creds: Option<&Credentials>) -> Option<Auth> {
    creds.map(|c| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Auth {
            user: c.user.clone(),
            pass: c.pass.clone(),
            token: format!("{nanos:032x}"),
        }
    })
}

/// Handle a login form POST (`user=..&pass=..`). On success, set the session
/// cookie; on failure, 401.
fn handle_login(mut request: Request, auth: &Auth) -> Result<()> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).ok();

    let user = query_param(Some(&body), "user").map(decode_percent);
    let pass = query_param(Some(&body), "pass").map(decode_percent);

    if user.as_deref() == Some(auth.user.as_str()) && pass.as_deref() == Some(auth.pass.as_str()) {
        let cookie = format!(
            "{SESSION_COOKIE}={}; Path=/; SameSite=Strict; Max-Age=86400",
            auth.token
        );
        respond(
            request,
            Response::from_string("ok").with_header(header("Set-Cookie", &cookie)),
        )
    } else {
        respond(
            request,
            Response::from_string("Incorrect username or password").with_status_code(401),
        )
    }
}

/// True if the request carries a `zap_session` cookie matching the token.
fn has_valid_session(request: &Request, auth: &Auth) -> bool {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))
        .and_then(|h| cookie_value(h.value.as_str(), SESSION_COOKIE))
        .map(|v| v == auth.token)
        .unwrap_or(false)
}

/// Pull a single cookie value out of a `Cookie:` header.
fn cookie_value<'a>(cookies: &'a str, name: &str) -> Option<&'a str> {
    cookies.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

// ---- URL / path utilities ----

fn split_query(url: &str) -> (String, Option<&str>) {
    match url.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query)),
        None => (url.to_string(), None),
    }
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Minimal percent-decoding (also turns '+' into space, matching form encoding).
fn decode_percent(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a client-supplied relative path against `root`, refusing anything
/// that would escape it. Because no `..` segment is ever accepted, the result
/// is always inside `root`. Empty / `.` segments are skipped, so `""` maps to
/// `root` itself.
fn resolve_within(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains('\\') {
            return None;
        }
        path.push(seg);
    }
    Some(path)
}

/// True if `name` is a single path component safe to create inside a folder.
fn is_plain_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && name != "."
        && name != ".."
}

/// JSON-encode a string (quotes + escapes). Enough for filenames.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Determine this machine's LAN IP by asking the OS which local address it
/// would use to reach an external host. No packets are actually sent.
pub fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}
