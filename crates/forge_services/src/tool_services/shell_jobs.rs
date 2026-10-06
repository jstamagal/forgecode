//! Shell jobs (T9): a command runs in its own process group with stdin from
//! `/dev/null` and a filtered environment; its streams are captured in the
//! background. A call waits for a window; a command still running then is
//! NOT killed — the call returns a handle the model can poll, wait on or kill.
//! Each stream keeps its head and tail in memory and, once it passes the
//! budget, goes whole to a file the result names.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::watch;

/// Finished jobs kept so a later poll can still report them.
const KEEP_FINISHED_JOBS: usize = 5;

/// Bytes snapped back when a cut lands mid-line.
const LINE_SNAP: usize = 200;

/// A capped accumulator for one stream: first and last `cap / 2` bytes in
/// memory; once the stream passes `cap`, every byte from the first goes to the
/// spill file.
#[derive(Debug)]
pub struct StreamCapture {
    cap: usize,
    spill_path: PathBuf,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    file: Option<std::fs::File>,
    spilled: bool,
    spill_failed: bool,
}

/// What of a stream's byte range fits a budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
    /// Bytes left out between head and tail, when any.
    pub omitted: Option<(usize, usize)>,
    pub total: usize,
    pub path: Option<PathBuf>,
}

impl StreamCapture {
    fn new(cap: usize, spill_path: PathBuf) -> Self {
        Self {
            cap,
            spill_path,
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
            file: None,
            spilled: false,
            spill_failed: false,
        }
    }

    fn head_cap(&self) -> usize {
        self.cap / 2
    }

    fn tail_cap(&self) -> usize {
        self.cap - self.head_cap()
    }

    fn dropped(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    fn start_spill(&mut self) {
        use std::io::Write;
        if self.spilled || self.spill_failed {
            return;
        }
        let result = (|| -> std::io::Result<std::fs::File> {
            if let Some(parent) = self.spill_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::File::create(&self.spill_path)?;
            file.write_all(&self.head)?;
            file.write_all(&self.tail)?;
            Ok(file)
        })();
        match result {
            Ok(file) => {
                self.file = Some(file);
                self.spilled = true;
            }
            Err(_) => self.spill_failed = true,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        use std::io::Write;
        if !self.spilled && self.total + chunk.len() > self.head_cap() + self.tail_cap() {
            self.start_spill();
        }
        if let Some(file) = self.file.as_mut()
            && file.write_all(chunk).is_err()
        {
            self.file = None;
            self.spilled = false;
            self.spill_failed = true;
        }
        self.total += chunk.len();
        let room = self.head_cap().saturating_sub(self.head.len());
        let (front, rest) = chunk.split_at(room.min(chunk.len()));
        self.head.extend_from_slice(front);
        if !rest.is_empty() {
            self.tail.extend_from_slice(rest);
            let excess = self.tail.len().saturating_sub(self.tail_cap());
            self.tail.drain(..excess);
        }
    }

    fn close(&mut self) {
        self.file = None;
    }

    /// Bytes `start..end` from memory or the spill file.
    fn read(&mut self, start: usize, end: usize) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};
        let end = end.min(self.total);
        if start >= end {
            return Vec::new();
        }
        if !self.dropped() {
            let all: Vec<u8> = self.head.iter().chain(self.tail.iter()).copied().collect();
            return all[start..end].to_vec();
        }
        let tail_start = self.total - self.tail.len();
        if end <= self.head.len() {
            return self.head[start..end].to_vec();
        }
        if start >= tail_start {
            return self.tail[start - tail_start..end - tail_start].to_vec();
        }
        if self.spilled {
            if let Some(file) = self.file.as_mut() {
                let _ = std::io::Write::flush(file);
            }
            if let Ok(mut file) = std::fs::File::open(&self.spill_path) {
                let mut buf = vec![0; end - start];
                if file.seek(SeekFrom::Start(start as u64)).is_ok()
                    && file.read_exact(&mut buf).is_ok()
                {
                    return buf;
                }
            }
        }
        let mut kept = self.head[start.min(self.head.len())..end.min(self.head.len())].to_vec();
        if end > tail_start {
            kept.extend_from_slice(
                &self.tail[start.max(tail_start) - tail_start..end - tail_start],
            );
        }
        kept
    }

    /// Bytes `start..end` cut to `budget`: half from each end of the range,
    /// split on UTF-8 boundaries and snapped to a nearby line break.
    pub fn excerpt(&mut self, start: usize, budget: usize, end: usize) -> Excerpt {
        let total = end.min(self.total);
        let start = start.min(total);
        if total - start <= budget {
            let path = self.spilled.then(|| self.spill_path.clone());
            return Excerpt {
                head: self.read(start, total),
                tail: Vec::new(),
                omitted: None,
                total,
                path,
            };
        }
        if !self.dropped() {
            self.start_spill();
        }
        let path = self.spilled.then(|| self.spill_path.clone());
        let head_n = budget / 2;
        let mut head = self.read(start, start + head_n);
        head.truncate(utf8_end(&head));
        if let Some(cut) = head
            .iter()
            .rposition(|b| *b == b'\n')
            .filter(|cut| *cut + LINE_SNAP >= head.len())
        {
            head.truncate(cut + 1);
        }
        let mut tail = self.read(total - (budget - head_n), total);
        let skip = utf8_start(&tail);
        tail.drain(..skip);
        if let Some(cut) = tail.iter().take(LINE_SNAP).position(|b| *b == b'\n')
            && cut + 1 < tail.len()
        {
            tail.drain(..=cut);
        }
        Excerpt {
            omitted: Some((start + head.len(), total - tail.len())),
            head,
            tail,
            total,
            path,
        }
    }
}

/// Length of the longest prefix that does not end inside a UTF-8 character.
fn utf8_end(buf: &[u8]) -> usize {
    let len = buf.len();
    for back in 1..=3.min(len) {
        let byte = buf[len - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let width = if byte >= 0xF0 {
            4
        } else if byte >= 0xE0 {
            3
        } else if byte >= 0xC0 {
            2
        } else {
            1
        };
        return if width > back { len - back } else { len };
    }
    len
}

/// Number of leading continuation bytes to skip.
fn utf8_start(buf: &[u8]) -> usize {
    buf.iter()
        .take(3)
        .take_while(|b| **b & 0xC0 == 0x80)
        .count()
}

/// Final state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
}

/// One shell job.
#[derive(Debug)]
pub struct ShellJob {
    pub id: String,
    pub command: String,
    pub shell: String,
    pub pid: u32,
    pub started: Instant,
    pub keep_ansi: bool,
    pub allowed: Vec<String>,
    pub present: Vec<String>,
    streams: [Arc<Mutex<StreamCapture>>; 2],
    exit: watch::Receiver<Option<Exit>>,
    /// Bytes of (stdout, stderr) already handed to the model.
    delivered: Mutex<(usize, usize)>,
}

impl ShellJob {
    /// The exit, once the process is gone and its streams drained.
    pub fn exit(&self) -> Option<Exit> {
        *self.exit.borrow()
    }

    /// Waits up to `window` for the job to finish.
    pub async fn wait(&self, window: Duration) -> Option<Exit> {
        let mut exit = self.exit.clone();
        let _ = tokio::time::timeout(window, exit.wait_for(Option::is_some)).await;
        self.exit()
    }

    /// SIGTERM, then SIGKILL after a grace period, to the whole group.
    pub async fn kill(&self) -> Option<Exit> {
        if self.exit().is_some() {
            return self.exit();
        }
        signal_group(self.pid, Signal::Term);
        if let Some(exit) = self.wait(Duration::from_secs(2)).await {
            return Some(exit);
        }
        signal_group(self.pid, Signal::Kill);
        self.wait(Duration::from_secs(5)).await
    }

    /// SIGKILL to the whole group without waiting (for a cancelled call).
    pub fn kill_now(&self) {
        signal_group(self.pid, Signal::Kill);
    }

    /// stdout and stderr to report (from the last reported byte when
    /// `since_last`), each cut to its share of `budget`.
    pub fn output(&self, budget: usize, since_last: bool) -> [Excerpt; 2] {
        let totals = [
            self.streams[0].lock().map(|s| s.total).unwrap_or(0),
            self.streams[1].lock().map(|s| s.total).unwrap_or(0),
        ];
        let mut delivered = self.delivered.lock().unwrap_or_else(|e| e.into_inner());
        let starts = if since_last { *delivered } else { (0, 0) };
        *delivered = (totals[0], totals[1]);
        let sizes = (totals[0] - starts.0, totals[1] - starts.1);
        let shares = split_budget(sizes, budget);
        [
            self.streams[0]
                .lock()
                .map(|mut s| s.excerpt(starts.0, shares.0, totals[0]))
                .unwrap_or_else(|_| empty_excerpt()),
            self.streams[1]
                .lock()
                .map(|mut s| s.excerpt(starts.1, shares.1, totals[1]))
                .unwrap_or_else(|_| empty_excerpt()),
        ]
    }
}

fn empty_excerpt() -> Excerpt {
    Excerpt {
        head: Vec::new(),
        tail: Vec::new(),
        omitted: None,
        total: 0,
        path: None,
    }
}

/// Shares `budget` between two streams: one under half keeps all of it.
fn split_budget(sizes: (usize, usize), budget: usize) -> (usize, usize) {
    let (out, err) = sizes;
    if out + err <= budget {
        return (out, err);
    }
    let half = budget / 2;
    if out <= half {
        (out, budget - out)
    } else if err <= half {
        (budget - err, err)
    } else {
        (half, budget - half)
    }
}

enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // The child is the leader of its own process group (setsid), so its pid
    // is the group id.
    // SAFETY: kill(2) with a negative pid signals the process group; it has
    // no memory-safety preconditions.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: Signal) {}

/// What a `run` needs.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub shell: String,
    pub env: BTreeMap<String, String>,
    pub allowed: Vec<String>,
    pub keep_ansi: bool,
    pub cap: usize,
    pub spill_dir: PathBuf,
}

/// All jobs of the process.
#[derive(Debug, Default)]
pub struct ShellJobs {
    jobs: Mutex<Vec<Arc<ShellJob>>>,
    sequence: AtomicU64,
}

impl ShellJobs {
    /// Starts a job.
    ///
    /// # Errors
    /// Returns an error when the shell cannot be spawned.
    pub fn start(&self, spec: RunSpec) -> anyhow::Result<Arc<ShellJob>> {
        let id = (self.sequence.fetch_add(1, Ordering::Relaxed) + 1).to_string();
        let shell_name = Path::new(&spec.shell)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut command = tokio::process::Command::new(&spec.shell);
        if matches!(shell_name.as_str(), "bash" | "zsh") {
            command.args(["-o", "pipefail"]);
        }
        command
            .arg("-c")
            .arg(&spec.command)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        // SAFETY: setsid() is async-signal-safe and only detaches the child
        // into its own session and process group.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child: Child = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("could not start {}: {error}", spec.shell))?;
        let pid = child.id().unwrap_or_default();
        let token: u32 = rand_token();
        let streams = [
            Arc::new(Mutex::new(StreamCapture::new(
                spec.cap,
                spec.spill_dir
                    .join(format!("shell-{id}-{token:08x}-stdout.log")),
            ))),
            Arc::new(Mutex::new(StreamCapture::new(
                spec.cap,
                spec.spill_dir
                    .join(format!("shell-{id}-{token:08x}-stderr.log")),
            ))),
        ];
        let (exit_tx, exit_rx) = watch::channel(None);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let readers = [
            spawn_reader(stdout, streams[0].clone()),
            spawn_reader(stderr, streams[1].clone()),
        ];
        let reader_streams = [streams[0].clone(), streams[1].clone()];
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            // Give readers a moment to drain; a backgrounded grandchild may
            // keep a pipe open forever.
            for reader in readers {
                let _ = tokio::time::timeout(Duration::from_millis(500), reader).await;
            }
            for stream in &reader_streams {
                if let Ok(mut stream) = stream.lock() {
                    stream.close();
                }
            }
            let _ = exit_tx.send(Some(Exit { code: status.and_then(|s| s.code()) }));
        });
        let present = spec.env.keys().cloned().collect();
        let job = Arc::new(ShellJob {
            id,
            command: spec.command,
            shell: spec.shell,
            pid,
            started: Instant::now(),
            keep_ansi: spec.keep_ansi,
            allowed: spec.allowed,
            present,
            streams,
            exit: exit_rx,
            delivered: Mutex::new((0, 0)),
        });
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.push(job.clone());
        let finished: Vec<String> = jobs
            .iter()
            .filter(|j| j.exit().is_some())
            .map(|j| j.id.clone())
            .collect();
        if finished.len() > KEEP_FINISHED_JOBS {
            let stale: Vec<&String> = finished[..finished.len() - KEEP_FINISHED_JOBS]
                .iter()
                .collect();
            jobs.retain(|j| !stale.contains(&&j.id));
        }
        Ok(job)
    }

    /// The job with `handle`, or the newest running job (else the newest).
    pub fn find(&self, handle: Option<&str>) -> Option<Arc<ShellJob>> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        match handle {
            Some(handle) => jobs.iter().find(|j| j.id == handle).cloned(),
            None => jobs
                .iter()
                .rev()
                .find(|j| j.exit().is_none())
                .or_else(|| jobs.last())
                .cloned(),
        }
    }

    /// Kills every running job (process exit).
    pub fn kill_all(&self) {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.iter().filter(|j| j.exit().is_none()) {
            signal_group(job.pid, Signal::Kill);
        }
    }
}

impl Drop for ShellJobs {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// A random token so spill files from different processes never collide (job
/// ids restart at 1 in every process).
fn rand_token() -> u32 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one(std::process::id()) as u32
}

fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    pipe: Option<R>,
    capture: Arc<Mutex<StreamCapture>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut pipe) = pipe else { return };
        let mut buf = vec![0u8; 65536];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if let Ok(mut capture) = capture.lock() {
                        capture.feed(&buf[..n]);
                    }
                }
            }
        }
    })
}

/// `$NAME` / `${NAME}` references in `command` that are set in this process's
/// environment but were filtered out of the command's.
pub fn filtered_references(
    command: &str,
    allowed: &[String],
    present: &HashMap<String, String>,
) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut names = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'{' {
                j += 1;
                if j < bytes.len() && (bytes[j] == b'#' || bytes[j] == b'!') {
                    j += 1;
                }
            }
            let start = j;
            if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                names.insert(command[start..j].to_string());
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    names
        .into_iter()
        .filter(|name| present.contains_key(name) && !allowed.contains(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn spec(command: &str, dir: &Path, cap: usize) -> RunSpec {
        let mut env = BTreeMap::new();
        env.insert(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        );
        RunSpec {
            command: command.to_string(),
            cwd: dir.to_path_buf(),
            shell: "bash".to_string(),
            env,
            allowed: vec!["PATH".to_string()],
            keep_ansi: false,
            cap,
            spill_dir: dir.join("spill"),
        }
    }

    fn text(excerpt: &Excerpt) -> String {
        bstr::ByteSlice::to_str_lossy(excerpt.head.as_slice()).into_owned()
    }

    #[tokio::test]
    async fn test_finishes_inside_window_with_pipefail() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = ShellJobs::default();
        let job = jobs
            .start(spec(
                "false | cat; echo out; echo err >&2",
                dir.path(),
                1000,
            ))
            .unwrap();

        let exit = job.wait(Duration::from_secs(10)).await;
        let [out, err] = job.output(1000, false);

        assert_eq!(exit, Some(Exit { code: Some(0) }));
        assert_eq!(
            (text(&out), text(&err)),
            ("out\n".to_string(), "err\n".to_string())
        );
        let job = jobs.start(spec("false | cat", dir.path(), 1000)).unwrap();
        assert_eq!(
            job.wait(Duration::from_secs(10)).await,
            Some(Exit { code: Some(1) })
        );
    }

    #[tokio::test]
    async fn test_outlives_window_then_poll_and_kill() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = ShellJobs::default();
        let job = jobs
            .start(spec("echo started; sleep 30; echo never", dir.path(), 1000))
            .unwrap();

        let first = job.wait(Duration::from_millis(300)).await;
        let [early, _] = job.output(1000, true);
        let found = jobs.find(None).unwrap();
        let killed = found.kill().await;

        assert_eq!(first, None);
        assert_eq!(text(&early), "started\n");
        assert!(killed.is_some());
        assert!(killed.unwrap().code.is_none(), "killed by signal");
    }

    #[tokio::test]
    async fn test_stdin_is_dev_null() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = ShellJobs::default();
        let job = jobs
            .start(spec("cat; echo done", dir.path(), 1000))
            .unwrap();
        let exit = job.wait(Duration::from_secs(10)).await;
        let [out, _] = job.output(1000, false);
        assert_eq!((exit.is_some(), text(&out)), (true, "done\n".to_string()));
    }

    #[tokio::test]
    async fn test_large_output_spills_whole_stream() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = ShellJobs::default();
        let job = jobs.start(spec("seq 1 20000", dir.path(), 400)).unwrap();
        job.wait(Duration::from_secs(10)).await;

        let [out, _] = job.output(400, false);

        let path = out.path.clone().expect("spilled");
        let whole = std::fs::read_to_string(path).unwrap();
        assert_eq!(whole.lines().count(), 20000);
        assert!(out.omitted.is_some());
        assert!(text(&out).starts_with("1\n2\n"));
        assert!(bstr::ByteSlice::to_str_lossy(out.tail.as_slice()).ends_with("20000\n"));
    }

    #[test]
    fn test_filtered_references_names_only_filtered_set_vars() {
        let mut present = HashMap::new();
        present.insert("SECRET".to_string(), "x".to_string());
        present.insert("PATH".to_string(), "/bin".to_string());
        let actual = filtered_references(
            "echo $SECRET ${PATH} $UNSET_THING",
            &["PATH".to_string()],
            &present,
        );
        let expected = vec!["SECRET".to_string()];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_utf8_boundaries() {
        let fixture = "aé".as_bytes();
        assert_eq!(utf8_end(&fixture[..2]), 1);
        assert_eq!(utf8_end(fixture), 3);
        assert_eq!(utf8_start(&fixture[2..]), 1);
    }
}
