//! Blocking herdr JSON API transport. herdr allows exactly one request per
//! connection, so every call opens a fresh UnixStream. Callers must run these
//! on a background thread, never on the UI thread.
//!
//! Supported on unix and windows (UnixStream). The wasm build compiles the
//! stub at the bottom of this file instead, whose calls all fail.

#[cfg(not(target_family = "wasm"))]
mod imp {
    use std::io::{BufRead, BufReader, Read, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
    #[cfg(windows)]
    use std::os::windows::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use anyhow::{Context, anyhow};
    use instant::Instant;
    use warp_core::safe_warn;

    use crate::herdr::protocol::*;

    const IO_TIMEOUT: Duration = Duration::from_secs(5);

    /// Bounded so a flooding peer parks the reader thread (blocking-send
    /// backpressure) instead of growing memory. Larger than herdr's
    /// 512-event replay burst so a normal subscribe never blocks.
    const EVENTS_CHANNEL_CAPACITY: usize = 1024;

    /// Reads one newline-terminated line from `stream` under a deadline,
    /// without SO_RCVTIMEO: clearing a read timeout afterwards is not
    /// portable (macOS returns EINVAL), and the stream must idle
    /// indefinitely after. The stream is switched to nonblocking for the
    /// read and restored before returning.
    fn read_line_with_deadline(stream: &UnixStream, deadline: Instant) -> anyhow::Result<Vec<u8>> {
        read_line_with_deadline_limit(stream, deadline, MAX_PEER_LINE_BYTES)
    }

    fn read_line_with_deadline_limit(
        stream: &UnixStream,
        deadline: Instant,
        max_line_bytes: u64,
    ) -> anyhow::Result<Vec<u8>> {
        stream
            .set_nonblocking(true)
            .context("set herdr subscription socket nonblocking")?;
        let result = read_line_with_deadline_inner(stream, deadline, max_line_bytes);
        stream
            .set_nonblocking(false)
            .context("restore herdr subscription socket blocking mode")?;
        result
    }

    fn read_line_with_deadline_inner(
        mut stream: &UnixStream,
        deadline: Instant,
        max_line_bytes: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            match stream.read(&mut byte) {
                Ok(0) => {
                    return Err(anyhow!("connection closed before a full line arrived"));
                }
                Ok(_) => {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("timed out waiting for a line"));
                    }
                    line.push(byte[0]);
                    let line_len = line.len() as u64;
                    if line_len > max_line_bytes || (line_len == max_line_bytes && byte[0] != b'\n')
                    {
                        return Err(anyhow!("herdr peer line exceeded {max_line_bytes} bytes"));
                    }
                    if byte[0] == b'\n' {
                        return Ok(line);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("timed out waiting for a line"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
    static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Debug)]
    pub struct HerdrApi {
        socket_path: PathBuf,
    }

    impl HerdrApi {
        pub fn at(socket_path: PathBuf) -> Self {
            Self { socket_path }
        }

        fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<ResponseResult> {
            let stream = UnixStream::connect(&self.socket_path)
                .with_context(|| format!("connect {}", self.socket_path.display()))?;
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            let id = format!("warp_{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed));
            let req = ApiRequest {
                id: id.clone(),
                method: method.to_string(),
                params,
            };
            let mut writer = stream.try_clone()?;
            writer.write_all(serde_json::to_string(&req)?.as_bytes())?;
            writer.write_all(b"\n")?;
            let mut line = String::new();
            // The take() cap turns a peer that streams an endless line into a
            // parse error instead of unbounded memory growth.
            BufReader::new(stream.take(MAX_PEER_LINE_BYTES)).read_line(&mut line)?;
            let (response_id, result) = match serde_json::from_str::<ApiResponse>(line.trim())
                .with_context(|| format!("parse herdr response to {method}"))?
            {
                ApiResponse::Success { id, result } => (id, Ok(*result)),
                ApiResponse::Error { id, error } => (
                    id,
                    Err(anyhow!(
                        "herdr {method} failed: {} ({})",
                        error.message,
                        error.code
                    )),
                ),
            };
            if response_id != id {
                return Err(anyhow!(
                    "herdr {method} response id {response_id} does not match request id {id}"
                ));
            }
            result
        }

        pub fn ping(&self) -> anyhow::Result<Pong> {
            match self.call("ping", serde_json::json!({}))? {
                ResponseResult::Pong(p) => Ok(p),
                other => Err(anyhow!("unexpected ping result: {other:?}")),
            }
        }

        pub fn session_snapshot(&self) -> anyhow::Result<SessionSnapshot> {
            match self.call("session.snapshot", serde_json::json!({}))? {
                ResponseResult::SessionSnapshot { snapshot } => Ok(snapshot),
                other => Err(anyhow!("unexpected snapshot result: {other:?}")),
            }
        }

        pub fn layout_export(&self, tab_id: &str) -> anyhow::Result<LayoutDescription> {
            match self.call("layout.export", serde_json::json!({ "tab_id": tab_id }))? {
                ResponseResult::LayoutExported { layout } => Ok(layout),
                other => Err(anyhow!("unexpected layout.export result: {other:?}")),
            }
        }

        pub fn tab_create(
            &self,
            workspace_id: &str,
            label: Option<&str>,
        ) -> anyhow::Result<(TabInfo, PaneInfo)> {
            let mut params = serde_json::json!({ "workspace_id": workspace_id, "focus": false });
            if let Some(label) = label {
                params["label"] = serde_json::Value::String(label.to_string());
            }
            match self.call("tab.create", params)? {
                ResponseResult::TabCreated { tab, root_pane } => Ok((tab, root_pane)),
                other => Err(anyhow!("unexpected tab.create result: {other:?}")),
            }
        }

        pub fn tab_close(&self, tab_id: &str) -> anyhow::Result<()> {
            self.call("tab.close", serde_json::json!({ "tab_id": tab_id }))
                .map(drop)
        }

        pub fn tab_rename(&self, tab_id: &str, label: &str) -> anyhow::Result<()> {
            self.call(
                "tab.rename",
                serde_json::json!({ "tab_id": tab_id, "label": label }),
            )
            .map(drop)
        }

        pub fn tab_focus(&self, tab_id: &str) -> anyhow::Result<()> {
            self.call("tab.focus", serde_json::json!({ "tab_id": tab_id }))
                .map(drop)
        }

        /// Moves the tab within its workspace's tab list. `insert_index` is
        /// 0-based and may equal the tab count (append at the end); the
        /// response carries the full post-move tab list, which the echoed
        /// tab.moved event also delivers.
        pub fn tab_move(&self, tab_id: &str, insert_index: usize) -> anyhow::Result<()> {
            self.call(
                "tab.move",
                serde_json::json!({ "tab_id": tab_id, "insert_index": insert_index }),
            )
            .map(drop)
        }

        pub fn pane_split(
            &self,
            target_pane_id: &str,
            direction: HerdrSplitDirection,
            ratio: Option<f32>,
        ) -> anyhow::Result<PaneInfo> {
            let mut params = serde_json::json!({
                "target_pane_id": target_pane_id,
                "direction": direction,
                "focus": false,
            });
            if let Some(r) = ratio {
                params["ratio"] = serde_json::json!(r);
            }
            match self.call("pane.split", params)? {
                ResponseResult::PaneInfo { pane } => Ok(pane),
                other => Err(anyhow!("unexpected pane.split result: {other:?}")),
            }
        }

        pub fn pane_close(&self, pane_id: &str) -> anyhow::Result<()> {
            self.call("pane.close", serde_json::json!({ "pane_id": pane_id }))
                .map(drop)
        }

        pub fn pane_rename(&self, pane_id: &str, label: Option<&str>) -> anyhow::Result<PaneInfo> {
            let mut params = serde_json::json!({ "pane_id": pane_id });
            if let Some(label) = label {
                params["label"] = serde_json::Value::String(label.to_string());
            }
            match self.call("pane.rename", params)? {
                ResponseResult::PaneInfo { pane } => Ok(pane),
                other => Err(anyhow!("unexpected pane.rename result: {other:?}")),
            }
        }

        pub fn pane_focus(&self, pane_id: &str) -> anyhow::Result<()> {
            self.call("pane.focus", serde_json::json!({ "pane_id": pane_id }))
                .map(drop)
        }

        pub fn layout_set_split_ratio(
            &self,
            tab_id: &str,
            path: &[bool],
            ratio: f32,
        ) -> anyhow::Result<()> {
            self.call(
                "layout.set_split_ratio",
                serde_json::json!({ "tab_id": tab_id, "path": path, "ratio": ratio }),
            )
            .map(drop)
        }

        /// Long-lived subscription. Reads the ack synchronously (a rejected
        /// subscription is an error here, not a silent stream close), then
        /// spawns a reader thread that parses each line into an AnyEvent and
        /// pushes it into the returned channel. The channel closes when the
        /// socket closes.
        ///
        /// Each id in `pane_ids` adds a pane-scoped `pane.agent_status_changed`
        /// subscription — the only delivery path for status transitions (they
        /// never arrive as pane.updated revisions).
        ///
        /// Half-alive-server detection is not this stream's job: the
        /// connection's heartbeat watchdog (connection.rs) requests snapshots
        /// on an interval and supersedes this stream through a resync when the
        /// server holds the socket open but stops answering.
        pub fn subscribe_events(
            &self,
            pane_ids: &[String],
        ) -> anyhow::Result<(async_channel::Receiver<AnyEvent>, EventStreamHandle)> {
            let stream = UnixStream::connect(&self.socket_path)
                .with_context(|| format!("connect {}", self.socket_path.display()))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            let mut subscriptions = serde_json::json!([
                {"type": "workspace.created"}, {"type": "workspace.renamed"},
                {"type": "workspace.closed"},
                {"type": "tab.created"}, {"type": "tab.closed"}, {"type": "tab.renamed"},
                {"type": "tab.moved"}, {"type": "tab.focused"},
                {"type": "pane.created"}, {"type": "pane.closed"}, {"type": "pane.updated"},
                {"type": "pane.focused"}, {"type": "pane.exited"},
                // Agent identity changes do NOT bump pane.updated revisions (only
                // metadata-token changes do); they arrive on this bare subscription.
                {"type": "pane.agent_detected"},
                {"type": "layout.updated"},
            ]);
            let Some(entries) = subscriptions.as_array_mut() else {
                unreachable!("subscriptions is a json! array literal");
            };
            // pane.agent_status_changed is pane-scoped (requires pane_id) and
            // rejects the whole subscribe without it.
            for pane_id in pane_ids {
                entries.push(serde_json::json!({
                    "type": "pane.agent_status_changed",
                    "pane_id": pane_id,
                }));
            }
            let id = format!(
                "warp_sub_{}",
                REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let req = ApiRequest {
                id,
                method: "events.subscribe".to_string(),
                params: serde_json::json!({ "subscriptions": subscriptions }),
            };
            let mut writer = stream.try_clone()?;
            writer.write_all(serde_json::to_string(&req)?.as_bytes())?;
            writer.write_all(b"\n")?;

            // The first line is the subscription_started ack — or, on a
            // rejected subscription, an error envelope. Read and check it
            // synchronously so a rejection surfaces as Err here instead of a
            // bare stream close that the consumer would reconnect-loop on
            // forever.
            let ack = read_line_with_deadline(&stream, Instant::now() + IO_TIMEOUT)
                .context("herdr events.subscribe ack")?;
            match serde_json::from_str::<ApiResponse>(String::from_utf8_lossy(&ack).trim())
                .context("parse herdr events.subscribe response")?
            {
                ApiResponse::Success { .. } => {}
                ApiResponse::Error { error, .. } => {
                    return Err(anyhow!(
                        "herdr events.subscribe failed: {} ({})",
                        error.message,
                        error.code
                    ));
                }
            }
            let shutdown = EventStreamHandle {
                stream: stream.try_clone().context("clone herdr events socket")?,
            };
            let reader = BufReader::new(stream);

            let (tx, rx) = async_channel::bounded(EVENTS_CHANNEL_CAPACITY);
            std::thread::Builder::new()
                .name("herdr-events".into())
                .spawn(move || {
                    let mut reader = reader;
                    let mut line = String::new();
                    loop {
                        match read_capped_line(&mut reader, &mut line) {
                            Ok(CappedLine::Line) => {}
                            Ok(CappedLine::Eof) => break,
                            Ok(CappedLine::Overlong) => {
                                log::warn!(
                                    "herdr: event line exceeded {MAX_PEER_LINE_BYTES} bytes; closing stream"
                                );
                                break;
                            }
                            Err(err) => {
                                log::warn!("herdr: event stream read failed: {err}");
                                break;
                            }
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<AnyEvent>(&line) {
                            Ok(ev) => {
                                // block_on(send) on the bounded channel parks
                                // this thread when full — the backpressure
                                // point (send_blocking is repo-disallowed:
                                // no wasm).
                                if warpui::r#async::block_on(tx.send(ev)).is_err() {
                                    break;
                                }
                            }
                            Err(err) => safe_warn!(
                                safe: ("herdr: unparseable event line"),
                                full: ("herdr: bad event line: {err}")
                            ),
                        }
                    }
                    // dropping tx closes the channel → consumer sees disconnect
                })
                .context("spawn herdr-events thread")?;
            Ok((rx, shutdown))
        }
    }

    /// Handle to a live events.subscribe socket. Shutting it down unblocks
    /// the reader thread (its read returns EOF, the thread exits, and the
    /// channel closes) and lets the server drop the subscription — without
    /// it, a superseded stream's thread + fd + server-side subscription
    /// leak until the server happens to close the socket.
    #[derive(Debug)]
    pub struct EventStreamHandle {
        stream: UnixStream,
    }

    impl EventStreamHandle {
        pub fn shutdown(&self) {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    impl Drop for EventStreamHandle {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    // ---- discovery ----

    #[derive(Debug, Clone, PartialEq)]
    pub struct DiscoveredSession {
        /// "default" or the named-session directory name.
        pub name: String,
        pub socket_path: PathBuf,
    }

    #[cfg(unix)]
    fn is_socket(path: &Path) -> bool {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        std::fs::metadata(path)
            .map(|m| {
                // Refuse sockets owned by another user: connecting would put
                // this user's terminal mirror in a foreign trust domain.
                // Cheap hardening — the socket normally lives under the
                // user's own config dir.
                m.file_type().is_socket()
                    && m.uid() == {
                        // SAFETY: getuid(2) is infallible
                        unsafe { libc::getuid() }
                    }
            })
            .unwrap_or(false)
    }

    #[cfg(windows)]
    fn is_socket(path: &Path) -> bool {
        // Windows has no FileTypeExt::is_socket; existence is enough — a wrong
        // path fails at connect time with context.
        path.exists()
    }

    /// Lists candidate herdr sessions under one herdr config dir.
    pub fn discover_sessions_in(config_dir: &Path) -> Vec<DiscoveredSession> {
        let mut out = Vec::new();
        let default_sock = config_dir.join("herdr.sock");
        if is_socket(&default_sock) {
            out.push(DiscoveredSession {
                name: "default".into(),
                socket_path: default_sock,
            });
        }
        if let Ok(entries) = std::fs::read_dir(config_dir.join("sessions")) {
            let mut named: Vec<DiscoveredSession> = entries
                .flatten()
                .filter_map(|entry| {
                    let sock = entry.path().join("herdr.sock");
                    is_socket(&sock).then(|| DiscoveredSession {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        socket_path: sock,
                    })
                })
                .collect();
            // readdir order is filesystem-dependent; sort so the
            // sessions[0] fallback in attach picks the same session every
            // run ("default", when present, always sorts first above).
            named.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(named);
        }
        out
    }

    /// Default herdr config dir: $XDG_CONFIG_HOME/herdr or ~/.config/herdr.
    /// $HERDR_SOCKET_PATH overrides discovery with a single candidate, honored
    /// verbatim (no is_socket gate — a bad path fails at connect time with
    /// context rather than silently attaching a different session). When
    /// $HERDR_SESSION is set, selection is restricted to that session name.
    pub fn discover_sessions() -> Vec<DiscoveredSession> {
        if let Ok(path) = std::env::var("HERDR_SOCKET_PATH") {
            return vec![DiscoveredSession {
                name: "default".into(),
                socket_path: PathBuf::from(path),
            }];
        }
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .map(|c| c.join("herdr"));
        let sessions = config_dir
            .map(|d| discover_sessions_in(&d))
            .unwrap_or_default();
        match std::env::var("HERDR_SESSION") {
            Ok(name) => sessions.into_iter().filter(|s| s.name == name).collect(),
            Err(_) => sessions,
        }
    }

    #[cfg(test)]
    mod deadline_tests {
        include!("socket_deadline_tests.rs");
    }
}

/// wasm stub: no UnixStream, so every call fails fast with the same public
/// API. Signatures (and param names) mirror the real implementation.
#[cfg(target_family = "wasm")]
mod imp {
    #![allow(unused_variables)]

    use std::path::{Path, PathBuf};

    use anyhow::anyhow;

    use crate::herdr::protocol::*;

    fn unsupported<T>() -> anyhow::Result<T> {
        Err(anyhow!("herdr is not supported on this platform"))
    }

    #[derive(Clone, Debug)]
    pub struct HerdrApi;

    impl HerdrApi {
        pub fn at(socket_path: PathBuf) -> Self {
            Self
        }

        pub fn ping(&self) -> anyhow::Result<Pong> {
            unsupported()
        }

        pub fn session_snapshot(&self) -> anyhow::Result<SessionSnapshot> {
            unsupported()
        }

        pub fn layout_export(&self, tab_id: &str) -> anyhow::Result<LayoutDescription> {
            unsupported()
        }

        pub fn tab_create(
            &self,
            workspace_id: &str,
            label: Option<&str>,
        ) -> anyhow::Result<(TabInfo, PaneInfo)> {
            unsupported()
        }

        pub fn tab_close(&self, tab_id: &str) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn tab_rename(&self, tab_id: &str, label: &str) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn tab_focus(&self, tab_id: &str) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn tab_move(&self, tab_id: &str, insert_index: usize) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn pane_split(
            &self,
            target_pane_id: &str,
            direction: HerdrSplitDirection,
            ratio: Option<f32>,
        ) -> anyhow::Result<PaneInfo> {
            unsupported()
        }

        pub fn pane_close(&self, pane_id: &str) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn pane_rename(&self, pane_id: &str, label: Option<&str>) -> anyhow::Result<PaneInfo> {
            let _ = (pane_id, label);
            unsupported()
        }

        pub fn pane_focus(&self, pane_id: &str) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn layout_set_split_ratio(
            &self,
            tab_id: &str,
            path: &[bool],
            ratio: f32,
        ) -> anyhow::Result<()> {
            unsupported()
        }

        pub fn subscribe_events(
            &self,
            pane_ids: &[String],
        ) -> anyhow::Result<(async_channel::Receiver<AnyEvent>, EventStreamHandle)> {
            unsupported()
        }
    }

    /// Stub mirroring the unix EventStreamHandle API.
    #[derive(Debug)]
    pub struct EventStreamHandle;

    impl EventStreamHandle {
        pub fn shutdown(&self) {}
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct DiscoveredSession {
        /// "default" or the named-session directory name.
        pub name: String,
        pub socket_path: PathBuf,
    }

    /// wasm has no herdr socket; discovery always comes up empty.
    pub fn discover_sessions() -> Vec<DiscoveredSession> {
        Vec::new()
    }

    pub fn discover_sessions_in(config_dir: &Path) -> Vec<DiscoveredSession> {
        Vec::new()
    }
}

pub use imp::*;

#[cfg(test)]
#[path = "socket_tests.rs"]
mod tests;
