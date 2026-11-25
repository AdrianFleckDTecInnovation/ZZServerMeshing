use super::{apply_checkpoint, invalid, Authority, Connection, Error, Id, Manifest, Record, Result};
use rusqlite::{OpenFlags, TransactionBehavior};
use std::{
    collections::BTreeMap,
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
    time::Duration,
};

pub(super) struct Checkpoint {
    pub group: Id,
    pub authority: Authority,
    pub base: u64,
    pub records: Vec<Record>,
    pub claims: BTreeMap<Id, Vec<Manifest>>,
}
struct Job {
    checkpoint: Checkpoint,
    reply: mpsc::Sender<Result<u64>>,
}

/// A separate WAL connection allows reads while SQLite synchronizes the durable log.
/// Concurrent checkpoints share an outer commit. Each keeps its own revision fence
/// and savepoint, so a rejected request leaves its neighbors independent.
pub(super) struct Writer {
    queue: Option<SyncSender<Job>>,
    thread: Option<JoinHandle<()>>,
}
impl Writer {
    pub fn open(path: &str) -> Result<Self> {
        let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        let (queue, jobs) = mpsc::sync_channel(64);
        let thread = thread::Builder::new()
            .name("mesh-checkpoints".into())
            .spawn(move || serve(&mut connection, jobs))?;
        Ok(Self {
            queue: Some(queue),
            thread: Some(thread),
        })
    }
    pub fn submit(&self, checkpoint: Checkpoint) -> Result<u64> {
        let (reply, answer) = mpsc::channel();
        self.queue
            .as_ref()
            .unwrap()
            .send(Job { checkpoint, reply })
            .map_err(|_| invalid("checkpoint writer stopped"))?;
        answer
            .recv()
            .map_err(|_| invalid("checkpoint writer stopped before commit"))?
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.queue.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn serve(connection: &mut Connection, incoming: Receiver<Job>) {
    let mut jobs = Vec::with_capacity(32);
    let mut answers = Vec::with_capacity(32);
    while let Ok(first) = incoming.recv() {
        jobs.push(first);
        // Drain only work that is already waiting. An isolated writer incurs no batching delay.
        jobs.extend(incoming.try_iter().take(31));
        let commit = (|| -> Result<()> {
            let mut tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for job in &mut jobs {
                let cp = &mut job.checkpoint;
                let mut savepoint = tx.savepoint()?;
                let result = apply_checkpoint(
                    &savepoint,
                    cp.group,
                    cp.authority,
                    cp.base,
                    std::mem::take(&mut cp.records),
                    &cp.claims,
                );
                if result.is_err() {
                    savepoint.rollback()?;
                }
                savepoint.commit()?;
                answers.push(result);
            }
            tx.commit()?;
            Ok(())
        })();
        match commit {
            Ok(()) => {
                for (job, answer) in jobs.drain(..).zip(answers.drain(..)) {
                    let _ = job.reply.send(answer);
                }
            }
            Err(e) => {
                answers.clear();
                for job in jobs.drain(..) {
                    let _ = job.reply.send(Err(Error::Io(std::io::Error::other(format!(
                        "checkpoint transaction failed: {e}"
                    )))));
                }
            }
        }
    }
}
