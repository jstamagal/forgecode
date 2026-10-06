//! Background `task` runs (T22): a handle the model polls, waits on or kills,
//! a result file, and a note for the next user message when a run finishes
//! without the model having read its result.
//!
//! Jobs belong to the conversation that started them; a conversation sees
//! only its own jobs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_domain::ConversationId;
use tokio::sync::watch;

/// Finished jobs kept for a later poll once their result was delivered.
const KEEP_FINISHED_JOBS: usize = 5;

/// How a background run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEnd {
    /// The assembled result text.
    Finished(String),
    /// Killed before it finished.
    Cancelled,
}

/// How a run ended and how long after its start.
type Ending = Option<(TaskEnd, Duration)>;

/// One background task run.
#[derive(Debug)]
pub struct TaskJob {
    pub id: String,
    pub owner: ConversationId,
    pub agent_id: String,
    pub count: usize,
    pub result_path: PathBuf,
    started: Instant,
    end: watch::Receiver<Ending>,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
    end_tx: Mutex<Option<watch::Sender<Ending>>>,
    /// The model has the result (poll, wait or kill).
    delivered: Mutex<bool>,
    /// A completion note went out.
    noted: Mutex<bool>,
    saved: Mutex<bool>,
}

impl TaskJob {
    /// Whether the run is still going.
    pub fn running(&self) -> bool {
        self.end.borrow().is_none()
    }

    /// How the run ended, once it has.
    pub fn end(&self) -> Option<TaskEnd> {
        self.end.borrow().as_ref().map(|(end, _)| end.clone())
    }

    /// Seconds since start (or until the end).
    pub fn elapsed_secs(&self) -> u64 {
        match self.end.borrow().as_ref() {
            Some((_, at)) => at.as_secs(),
            None => self.started.elapsed().as_secs(),
        }
    }

    /// Waits up to `timeout` (forever when `None`) for the run to end.
    pub async fn wait(&self, timeout: Option<Duration>) {
        let mut end = self.end.clone();
        let wait = end.wait_for(Option::is_some);
        match timeout {
            Some(timeout) => {
                let _ = tokio::time::timeout(timeout, wait).await;
            }
            None => {
                let _ = wait.await;
            }
        }
    }

    /// Stops the run.
    pub fn kill(&self) {
        if let Some(abort) = self.abort.lock().unwrap_or_else(|e| e.into_inner()).take() {
            abort.abort();
        }
        self.settle(TaskEnd::Cancelled);
    }

    fn settle(&self, end: TaskEnd) {
        let sender = self.end_tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(sender) = sender {
            if let TaskEnd::Finished(text) = &end {
                let saved = save(&self.result_path, text);
                *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = saved;
            }
            let _ = sender.send(Some((end, self.started.elapsed())));
        }
    }

    fn mark_delivered(&self) {
        *self.delivered.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    fn saved(&self) -> bool {
        *self.saved.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn save(path: &std::path::Path, text: &str) -> bool {
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = path.with_extension("tmp");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, path)
    };
    write().is_ok()
}

/// All background task runs of the process.
#[derive(Debug, Default)]
pub struct TaskJobs {
    jobs: Mutex<Vec<Arc<TaskJob>>>,
    sequence: AtomicU64,
}

impl TaskJobs {
    /// Starts `run` in the background for `owner`.
    ///
    /// # Arguments
    /// * `owner` - The conversation that started it.
    /// * `agent_id` - The worker agent.
    /// * `count` - Number of worker tasks.
    /// * `results_dir` - Where the result file goes.
    /// * `run` - The fan-out; resolves to the assembled result text.
    pub fn start<F>(
        &self,
        owner: ConversationId,
        agent_id: &str,
        count: usize,
        results_dir: &std::path::Path,
        run: F,
    ) -> Arc<TaskJob>
    where
        F: std::future::Future<Output = String> + Send + 'static,
    {
        let id = format!("t{}", self.sequence.fetch_add(1, Ordering::Relaxed) + 1);
        let token = std::process::id();
        let (end_tx, end_rx) = watch::channel(None);
        let job = Arc::new(TaskJob {
            result_path: results_dir.join(format!("task-{id}-{token:x}.txt")),
            id,
            owner,
            agent_id: agent_id.to_string(),
            count,
            started: Instant::now(),
            end: end_rx,
            abort: Mutex::new(None),
            end_tx: Mutex::new(Some(end_tx)),
            delivered: Mutex::new(false),
            noted: Mutex::new(false),
            saved: Mutex::new(false),
        });
        let settle = job.clone();
        let handle = tokio::spawn(async move {
            let text = run.await;
            settle.settle(TaskEnd::Finished(text));
        });
        *job.abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.abort_handle());
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.push(job.clone());
        prune(&mut jobs);
        job
    }

    /// `owner`'s job `handle`; without one, its newest running job, else its
    /// newest job.
    pub fn find(&self, owner: &ConversationId, handle: Option<&str>) -> Option<Arc<TaskJob>> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let mine: Vec<&Arc<TaskJob>> = jobs.iter().filter(|job| &job.owner == owner).collect();
        match handle {
            Some(handle) => mine
                .into_iter()
                .find(|job| job.id == handle.trim())
                .cloned(),
            None => mine
                .iter()
                .rev()
                .find(|job| job.running())
                .or_else(|| mine.last())
                .map(|job| (*job).clone()),
        }
    }

    /// One note per job of `owner` that finished since the last call and
    /// whose result the model has not read.
    pub fn completion_notes(&self, owner: &ConversationId) -> Vec<String> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let mut notes = Vec::new();
        for job in jobs.iter().filter(|job| &job.owner == owner) {
            if job.running() || *job.delivered.lock().unwrap_or_else(|e| e.into_inner()) {
                continue;
            }
            let mut noted = job.noted.lock().unwrap_or_else(|e| e.into_inner());
            if *noted {
                continue;
            }
            *noted = true;
            let note = match job.end() {
                Some(TaskEnd::Cancelled) => format!(
                    "Background task {} (agent `{}`) was cancelled after {}s and has no result.",
                    job.id,
                    job.agent_id,
                    job.elapsed_secs()
                ),
                _ => format!(
                    "Background task {} (agent `{}`, {} {}) finished after {}s. {}; read it, or call task with action=\"poll\", handle=\"{}\".",
                    job.id,
                    job.agent_id,
                    job.count,
                    if job.count == 1 { "task" } else { "tasks" },
                    job.elapsed_secs(),
                    if job.saved() {
                        format!("Its result is at {}", job.result_path.display())
                    } else {
                        "Its result could not be saved to a file".to_string()
                    },
                    job.id
                ),
            };
            notes.push(note);
        }
        notes
    }
}

fn prune(jobs: &mut Vec<Arc<TaskJob>>) {
    let settled: Vec<String> = jobs
        .iter()
        .filter(|job| {
            !job.running()
                && (*job.delivered.lock().unwrap_or_else(|e| e.into_inner())
                    || *job.noted.lock().unwrap_or_else(|e| e.into_inner()))
        })
        .map(|job| job.id.clone())
        .collect();
    if settled.len() > KEEP_FINISHED_JOBS {
        let stale = &settled[..settled.len() - KEEP_FINISHED_JOBS];
        jobs.retain(|job| !stale.contains(&job.id));
    }
}

/// Text returned when a background run starts.
pub fn started_text(job: &TaskJob) -> String {
    let plural = if job.count == 1 { "task" } else { "tasks" };
    [
        format!(
            "task running in the background (handle {}): {} {plural} for agent `{}`. It keeps running while you work.",
            job.id, job.count, job.agent_id
        ),
        format!(
            "Poll it with action=\"poll\", handle=\"{0}\", action=\"wait\", handle=\"{0}\", timeout=N to block for it, or action=\"kill\", handle=\"{0}\" to stop it.",
            job.id
        ),
        format!(
            "When it finishes its result is written to {}, and the next user message says so unless you have read it by then.",
            job.result_path.display()
        ),
        format!("HANDLE {} RUNNING", job.id),
    ]
    .join("\n")
}

/// The answer to poll, wait or kill once any waiting is over.
pub fn report(job: &TaskJob, kill: bool, was_running: bool) -> String {
    if kill && was_running {
        job.mark_delivered();
        return format!("killed task {} after {}s", job.id, job.elapsed_secs());
    }
    match job.end() {
        None => format!(
            "task {} still running after {}s.\nHANDLE {} RUNNING",
            job.id,
            job.elapsed_secs(),
            job.id
        ),
        Some(TaskEnd::Cancelled) => {
            job.mark_delivered();
            format!(
                "task {} was cancelled after {}s; it has no result",
                job.id,
                job.elapsed_secs()
            )
        }
        Some(TaskEnd::Finished(text)) => {
            job.mark_delivered();
            let verb = if kill {
                "had already finished"
            } else {
                "finished"
            };
            let where_ = if job.saved() {
                format!("; the result is also at {}", job.result_path.display())
            } else {
                String::new()
            };
            format!(
                "task {} {verb} after {}s{where_}\n{text}",
                job.id,
                job.elapsed_secs()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn test_background_job_finishes_saves_and_notes_once() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = TaskJobs::default();
        let owner = ConversationId::generate();
        let job = jobs.start(owner, "sage", 1, dir.path(), async { "done".to_string() });

        job.wait(Some(Duration::from_secs(5))).await;

        let saved = std::fs::read_to_string(&job.result_path).unwrap();
        let notes = jobs.completion_notes(&owner);
        let again = jobs.completion_notes(&owner);
        assert_eq!(saved, "done");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("Background task t1 (agent `sage`, 1 task) finished"),
            "{}",
            notes[0]
        );
        assert_eq!(again, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_poll_running_then_kill() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = TaskJobs::default();
        let owner = ConversationId::generate();
        let job = jobs.start(owner, "forge", 2, dir.path(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            "never".to_string()
        });

        let polled = report(&job, false, true);
        let found = jobs.find(&owner, None).unwrap();
        let was_running = found.running();
        found.kill();
        let killed = report(&found, true, was_running);

        assert!(polled.ends_with("HANDLE t1 RUNNING"), "{polled}");
        assert!(killed.starts_with("killed task t1 after"), "{killed}");
        assert_eq!(found.end(), Some(TaskEnd::Cancelled));
    }

    #[tokio::test]
    async fn test_jobs_are_scoped_to_their_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = TaskJobs::default();
        let mine = ConversationId::generate();
        let other = ConversationId::generate();
        jobs.start(mine, "sage", 1, dir.path(), async { "x".to_string() });

        let actual = (
            jobs.find(&mine, Some("t1")).is_some(),
            jobs.find(&other, Some("t1")).is_some(),
        );

        let expected = (true, false);
        assert_eq!(actual, expected);
    }
}
