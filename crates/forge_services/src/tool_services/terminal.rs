//! T15: persistent PTY sessions (`terminal_session`) and screen images
//! (`terminal_snapshot`), on `portable-pty` + `vt100`.
//!
//! Sessions are keyed by conversation and name; a background thread feeds
//! the PTY's output into a vt100 parser. Every session of a conversation is
//! killed when the conversation's engine is dropped (process exit) or with
//! `stop`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};

/// Named keys and the bytes they send.
pub fn key_sequence(token: &str) -> Option<&'static str> {
    Some(match token.to_lowercase().as_str() {
        "enter" => "\r",
        "tab" => "\t",
        "esc" => "\x1b",
        "backspace" => "\x7f",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pgup" => "\x1b[5~",
        "pgdn" => "\x1b[6~",
        "delete" => "\x1b[3~",
        "ctrl-c" => "\x03",
        "ctrl-d" => "\x04",
        "ctrl-l" => "\x0c",
        "ctrl-x" => "\x18",
        "ctrl-z" => "\x1a",
        "f1" => "\x1bOP",
        "f2" => "\x1bOQ",
        "f3" => "\x1bOR",
        "f4" => "\x1bOS",
        "f5" => "\x1b[15~",
        "f6" => "\x1b[17~",
        "f7" => "\x1b[18~",
        "f8" => "\x1b[19~",
        "f9" => "\x1b[20~",
        "f10" => "\x1b[21~",
        "f11" => "\x1b[23~",
        "f12" => "\x1b[24~",
        "comma" => ",",
        _ => return None,
    })
}

struct Session {
    command: String,
    cwd: PathBuf,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Bytes received; lets `drain` wait until output stops arriving.
    received: Arc<Mutex<u64>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    previous: Mutex<Option<Vec<String>>>,
    snapshots: Mutex<u32>,
}

impl Session {
    fn lines(&self) -> Vec<String> {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let (_, cols) = screen.size();
        screen
            .rows(0, cols)
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    fn exit_status(&self) -> Option<Option<i64>> {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match child.try_wait() {
            Ok(Some(status)) => Some(Some(status.exit_code() as i64)),
            Ok(None) => None,
            Err(_) => Some(None),
        }
    }

    /// Waits `wait` for output, returning early once output has been quiet
    /// for 100 ms after some arrived.
    fn drain(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let start = *self.received.lock().unwrap_or_else(|e| e.into_inner());
        let mut last = start;
        let mut quiet_since = Instant::now();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            let now = *self.received.lock().unwrap_or_else(|e| e.into_inner());
            if now != last {
                last = now;
                quiet_since = Instant::now();
            } else if now != start && quiet_since.elapsed() >= Duration::from_millis(100) {
                break;
            }
        }
    }

    fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pid) = child.process_id() {
            // SAFETY: signals the session's own process group (the PTY child
            // is a session leader).
            unsafe {
                libc::killpg(pid as i32, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_millis(150));
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// What starting a session needs.
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub shell: String,
    pub env: BTreeMap<String, String>,
    pub jail: Option<forge_domain::Jail>,
}

/// Every terminal session of the process, by (conversation, name).
#[derive(Default)]
pub struct Terminals {
    sessions: Mutex<BTreeMap<(String, String), Arc<Session>>>,
}

impl std::fmt::Debug for Terminals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminals").finish_non_exhaustive()
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        for session in self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            session.kill();
        }
    }
}

impl Terminals {
    fn get(&self, owner: &str, name: &str) -> anyhow::Result<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(owner.to_string(), name.to_string()))
            .cloned()
            .with_context(|| format!("no terminal session named {name:?}; use action=start first"))
    }

    /// Starts (or replaces) session `name` of `owner`.
    ///
    /// # Errors
    /// Returns an error when the PTY or the command cannot be started.
    pub fn start(
        &self,
        owner: &str,
        name: &str,
        spec: StartSpec,
        wait: Duration,
    ) -> anyhow::Result<Value> {
        if !spec.cwd.is_dir() {
            bail!("no such directory: {}", spec.cwd.display());
        }
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("could not open a pty")?;
        let shell_argv = vec![spec.shell.clone(), "-c".to_string(), spec.command.clone()];
        let argv = match &spec.jail {
            Some(jail) => {
                let path_env = spec.env.get("PATH").cloned().unwrap_or_default();
                let mut wrapped = vec![jail.bwrap.display().to_string()];
                wrapped.extend(jail.argv(&shell_argv, &spec.cwd, &path_env));
                wrapped
            }
            None => shell_argv,
        };
        let mut command = CommandBuilder::new(&argv[0]);
        command.args(&argv[1..]);
        command.cwd(&spec.cwd);
        command.env_clear();
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLUMNS", spec.cols.to_string());
        command.env("LINES", spec.rows.to_string());
        let child = pty
            .slave
            .spawn_command(command)
            .context("could not start the terminal command")?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(spec.rows, spec.cols, 0)));
        let received = Arc::new(Mutex::new(0u64));
        {
            let parser = parser.clone();
            let received = received.clone();
            // Keeps the master side alive as long as the reader runs.
            let master = pty.master;
            std::thread::spawn(move || {
                let _master = master;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .process(&buf[..n]);
                            *received.lock().unwrap_or_else(|e| e.into_inner()) += n as u64;
                        }
                    }
                }
            });
        }
        let session = Arc::new(Session {
            command: spec.command.clone(),
            cwd: spec.cwd.clone(),
            parser,
            received,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            previous: Mutex::new(None),
            snapshots: Mutex::new(0),
        });
        let old = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((owner.to_string(), name.to_string()), session.clone());
        if let Some(old) = old {
            old.kill();
        }
        Ok(observe(
            &session,
            name,
            &format!("started {}", spec.command),
            wait,
        ))
    }

    /// Sends comma-separated `keys` and observes for `wait`.
    ///
    /// # Errors
    /// Returns an error for an unknown session or one that has exited.
    pub fn send(
        &self,
        owner: &str,
        name: &str,
        keys: &str,
        wait: Duration,
    ) -> anyhow::Result<Value> {
        let session = self.get(owner, name)?;
        if session.exit_status().is_some() {
            bail!("terminal session {name:?} has exited");
        }
        // Output that arrived before the keys is not part of the comparison.
        session.drain(Duration::from_millis(0));
        *session.previous.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.lines());
        let mut sent = Vec::new();
        {
            let mut writer = session.writer.lock().unwrap_or_else(|e| e.into_inner());
            for token in keys.split(',').filter(|t| !t.is_empty()) {
                let bytes = key_sequence(token).unwrap_or(token);
                writer.write_all(bytes.as_bytes())?;
                writer.flush()?;
                sent.push(token.to_string());
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        Ok(observe(&session, name, &format!("sent {sent:?}"), wait))
    }

    /// Observes the screen after `wait`.
    ///
    /// # Errors
    /// Returns an error for an unknown session.
    pub fn look(&self, owner: &str, name: &str, wait: Duration) -> anyhow::Result<Value> {
        let session = self.get(owner, name)?;
        Ok(observe(&session, name, "looked", wait))
    }

    /// Stops and forgets a session.
    ///
    /// # Errors
    /// Returns an error for an unknown session.
    pub fn stop(&self, owner: &str, name: &str) -> anyhow::Result<Value> {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(owner.to_string(), name.to_string()))
            .with_context(|| format!("no terminal session named {name:?}"))?;
        session.kill();
        Ok(json!({"session": name, "stopped": true}))
    }

    /// The sessions of `owner`.
    pub fn list(&self, owner: &str) -> Value {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let rows: Vec<Value> = sessions
            .iter()
            .filter(|((o, _), _)| o == owner)
            .map(|((_, name), session)| {
                json!({
                    "session": name,
                    "command": session.command,
                    "cwd": session.cwd,
                    "alive": session.exit_status().is_none(),
                })
            })
            .collect();
        json!({"sessions": rows})
    }

    /// Renders the screen of a session to a PNG under `dir`; resets the
    /// change baseline.
    ///
    /// # Errors
    /// Returns an error for an unknown session or a render/write failure.
    pub fn snapshot(
        &self,
        owner: &str,
        name: &str,
        wait: Duration,
        dir: &Path,
    ) -> anyhow::Result<(PathBuf, Vec<u8>)> {
        let session = self.get(owner, name)?;
        session.drain(wait);
        let png = {
            let parser = session.parser.lock().unwrap_or_else(|e| e.into_inner());
            render_png(parser.screen())?
        };
        *session.previous.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.lines());
        let n = {
            let mut n = session.snapshots.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            *n
        };
        std::fs::create_dir_all(dir)?;
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let path = dir.join(format!("{safe}-{n:02}.png"));
        std::fs::write(&path, &png)?;
        Ok((path, png))
    }
}

fn observe(session: &Session, name: &str, did: &str, wait: Duration) -> Value {
    session.drain(wait);
    let lines = session.lines();
    let (row, col) = {
        let parser = session.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().cursor_position()
    };
    let mut payload = json!({
        "session": name,
        "did": did,
        "lines": lines,
        "nonblank_lines": lines.iter().filter(|l| !l.trim().is_empty()).count(),
        "cursor": [row, col],
    });
    let mut previous = session.previous.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(before) = previous.as_ref() {
        let changed = before.iter().zip(&lines).filter(|(a, b)| a != b).count();
        payload["lines_changed"] = json!(changed);
        payload["screen_responded"] = json!(changed > 0);
    }
    *previous = Some(lines);
    match session.exit_status() {
        None => payload["still_running"] = json!(true),
        Some(code) => {
            payload["still_running"] = json!(false);
            payload["exit_status"] = json!(code);
        }
    }
    payload
}

const FONT_CANDIDATES: [&str; 5] = [
    "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
];

fn rgb(color: vt100::Color, fallback: [u8; 3]) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    match color {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(i) if (i as usize) < 16 => ANSI[i as usize],
        vt100::Color::Idx(i) if i >= 232 => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
        vt100::Color::Idx(i) => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            [step(i / 36), step((i / 6) % 6), step(i % 6)]
        }
        vt100::Color::Rgb(r, g, b) => [r, g, b],
    }
}

/// Renders `screen` as a PNG: one monospace cell per character with its
/// colours, and a cursor outline.
///
/// # Errors
/// Returns an error when no monospace font is installed.
pub fn render_png(screen: &vt100::Screen) -> anyhow::Result<Vec<u8>> {
    use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

    let font_bytes = FONT_CANDIDATES
        .iter()
        .find_map(|path| std::fs::read(path).ok())
        .context("no monospace font found for terminal_snapshot")?;
    let font = FontVec::try_from_vec(font_bytes).context("unreadable font")?;
    let scale = PxScale::from(16.0);
    let scaled = font.as_scaled(scale);
    let cell_w = scaled.h_advance(font.glyph_id('M')).ceil().max(1.0) as u32;
    let cell_h = (16.0f32 * 1.35).ceil() as u32;
    let (rows, cols) = screen.size();
    let background = [12u8, 12, 12];
    let foreground = [222u8, 222, 222];
    let mut image = image::RgbImage::from_pixel(
        cell_w * cols as u32 + 8,
        cell_h * rows as u32 + 8,
        image::Rgb(background),
    );
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let mut fg = rgb(cell.fgcolor(), foreground);
            let mut bg = rgb(cell.bgcolor(), background);
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let (px, py) = (4 + col as u32 * cell_w, 4 + row as u32 * cell_h);
            if bg != background {
                for y in py..py + cell_h {
                    for x in px..px + cell_w {
                        image.put_pixel(x, y, image::Rgb(bg));
                    }
                }
            }
            let Some(ch) = cell.contents().chars().next().filter(|c| *c != ' ') else {
                continue;
            };
            let glyph = font.glyph_id(ch).with_scale_and_position(
                scale,
                ab_glyph::point(px as f32, py as f32 + scaled.ascent()),
            );
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|x, y, coverage| {
                    let (x, y) = (
                        bounds.min.x as i64 + x as i64,
                        bounds.min.y as i64 + y as i64,
                    );
                    if x < 0 || y < 0 || x >= image.width() as i64 || y >= image.height() as i64 {
                        return;
                    }
                    let pixel = image.get_pixel_mut(x as u32, y as u32);
                    for (channel, value) in pixel.0.iter_mut().zip(fg) {
                        *channel =
                            (*channel as f32 * (1.0 - coverage) + value as f32 * coverage) as u8;
                    }
                });
            }
        }
    }
    if !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        if row < rows && col < cols {
            let (px, py) = (4 + col as u32 * cell_w, 4 + row as u32 * cell_h);
            for x in px..px + cell_w {
                image.put_pixel(x, py, image::Rgb([255, 255, 255]));
                image.put_pixel(x, py + cell_h - 1, image::Rgb([255, 255, 255]));
            }
            for y in py..py + cell_h {
                image.put_pixel(px, y, image::Rgb([255, 255, 255]));
                image.put_pixel(px + cell_w - 1, y, image::Rgb([255, 255, 255]));
            }
        }
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn spec(command: &str, dir: &Path) -> StartSpec {
        StartSpec {
            command: command.to_string(),
            cwd: dir.to_path_buf(),
            cols: 40,
            rows: 6,
            shell: "/bin/sh".to_string(),
            env: [(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )]
            .into(),
            jail: None,
        }
    }

    #[test]
    fn test_interactive_program_reads_keys_and_screen_changes() {
        let dir = tempfile::tempdir().unwrap();
        let terminals = Terminals::default();
        let started = terminals
            .start(
                "c1",
                "main",
                spec(
                    "printf 'name? '; read n; echo \"hello $n\"; sleep 5",
                    dir.path(),
                ),
                Duration::from_millis(500),
            )
            .unwrap();
        let sent = terminals
            .send("c1", "main", "ape,enter", Duration::from_millis(800))
            .unwrap();

        assert_eq!(started["lines"][0], "name?");
        assert!(
            sent["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l == "hello ape"),
            "{sent}"
        );
        assert_eq!(sent["screen_responded"], true);
        assert_eq!(sent["still_running"], true);
        terminals.stop("c1", "main").unwrap();
        assert!(terminals.look("c1", "main", Duration::ZERO).is_err());
    }

    #[test]
    fn test_exit_status_and_list_are_scoped_to_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let terminals = Terminals::default();
        terminals
            .start(
                "c1",
                "job",
                spec("exit 3", dir.path()),
                Duration::from_millis(500),
            )
            .unwrap();

        let looked = terminals
            .look("c1", "job", Duration::from_millis(300))
            .unwrap();
        let mine = terminals.list("c1");
        let theirs = terminals.list("c2");

        assert_eq!(
            (
                looked["still_running"].clone(),
                looked["exit_status"].clone()
            ),
            (json!(false), json!(3))
        );
        assert_eq!(mine["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(theirs["sessions"].as_array().unwrap().len(), 0);
        assert!(terminals.send("c1", "job", "x", Duration::ZERO).is_err());
    }

    #[test]
    fn test_snapshot_writes_a_png() {
        let dir = tempfile::tempdir().unwrap();
        let terminals = Terminals::default();
        terminals
            .start(
                "c1",
                "s",
                spec("printf '\\033[31mred\\033[0m'; sleep 5", dir.path()),
                Duration::from_millis(500),
            )
            .unwrap();

        let (path, png) = terminals
            .snapshot(
                "c1",
                "s",
                Duration::from_millis(100),
                &dir.path().join("shots"),
            )
            .unwrap();

        assert_eq!(&png[1..4], b"PNG");
        assert!(path.ends_with("s-01.png"));
        terminals.stop("c1", "s").unwrap();
    }

    #[test]
    fn test_named_keys() {
        let actual = (
            key_sequence("Enter"),
            key_sequence("ctrl-x"),
            key_sequence("f12"),
            key_sequence("q"),
        );
        assert_eq!(actual, (Some("\r"), Some("\x18"), Some("\x1b[24~"), None));
    }
}
