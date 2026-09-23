//! The ticket journal -- v4's one piece of mandatory durable state (§17.5).
//!
//! §46.1: "Recovery code must reconcile all in-flight tickets from durable
//! state before new live dispatch is re-enabled." B-6 recorded that the legacy
//! system has no such record at all, so a crash between sign and receipt leaves
//! an outcome nobody can classify.
//!
//! Append-only, one JSON object per line. Not a database: recovery reads the
//! whole file once at startup and never again, and the write path must not do
//! anything more interesting than `write` + sometimes `fsync`.

use apex_types::ids::TicketId;
use apex_types::ticket::{OpportunityTicket, TicketOutcome, TicketStatus};
use apex_types::time::UnixNanos;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum JournalEntry {
    Admitted { id: TicketId, at: UnixNanos, ticket: Box<OpportunityTicket> },
    Advanced { id: TicketId, at: UnixNanos, to: TicketStatus },
    Closed { id: TicketId, at: UnixNanos, outcome: Box<TicketOutcome> },
}

impl JournalEntry {
    pub const fn ticket_id(&self) -> TicketId {
        match self {
            Self::Admitted { id, .. } | Self::Advanced { id, .. } | Self::Closed { id, .. } => *id,
        }
    }

    /// §17.5: writes `fsync` from `Authorized` onward. Before authorization a
    /// ticket carries no capital risk, so buffering is safe.
    ///
    /// This is sufficient rather than merely cheap, and the reason is a property
    /// of `fsync` rather than of this type: `fsync` flushes the *whole file*, so
    /// the first durable write after a run of buffered ones makes all of them
    /// durable too. A ticket that reached `Authorized` therefore has its
    /// `Admitted` record on disk as well, which is what recovery needs to know
    /// what the ticket was.
    pub const fn requires_durable_write(&self) -> bool {
        match self {
            Self::Admitted { .. } => false,
            Self::Advanced { to, .. } => to.requires_durable_write(),
            // A terminal outcome always syncs. Losing one turns a classified
            // failure into an unclassified one, which is the single thing
            // §46.1 forbids.
            Self::Closed { .. } => true,
        }
    }
}

pub trait Journal: Send + Sync {
    fn append(&self, entry: &JournalEntry) -> io::Result<()>;
    /// Every entry ever written, in write order. Recovery's only read.
    fn replay(&self) -> io::Result<Vec<JournalEntry>>;
    /// How many `fsync` calls this journal has made. Exposed because "syncs at
    /// the right moments" is otherwise an untestable claim about a side effect
    /// with no return value.
    fn sync_count(&self) -> u64;
}

/// A poisoned lock means a panic happened while a journal append was in
/// progress. Recovering the inner value rather than propagating the poison is
/// deliberate: the append is a push onto a `Vec` or a `write_all` on an
/// `O_APPEND` file, neither of which leaves a half-updated structure, and a
/// journal that refuses to record *because* something already went wrong is a
/// journal that fails exactly when it is needed.
fn recover<T>(r: std::sync::LockResult<T>) -> T {
    match r {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Debug, Default)]
pub struct InMemoryJournal {
    entries: Mutex<Vec<JournalEntry>>,
    syncs: Mutex<u64>,
}

impl InMemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Journal for InMemoryJournal {
    fn append(&self, entry: &JournalEntry) -> io::Result<()> {
        recover(self.entries.lock()).push(entry.clone());
        if entry.requires_durable_write() {
            *recover(self.syncs.lock()) += 1;
        }
        Ok(())
    }
    fn replay(&self) -> io::Result<Vec<JournalEntry>> {
        Ok(recover(self.entries.lock()).clone())
    }
    fn sync_count(&self) -> u64 {
        *recover(self.syncs.lock())
    }
}

/// The real one.
#[derive(Debug)]
pub struct FileJournal {
    path: PathBuf,
    file: Mutex<File>,
    syncs: Mutex<u64>,
}

impl FileJournal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().create(true).append(true).read(true).open(&path)?;
        Ok(Self { path, file: Mutex::new(file), syncs: Mutex::new(0) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Journal for FileJournal {
    fn append(&self, entry: &JournalEntry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        let mut f = recover(self.file.lock());
        // One `write_all` per entry, on a file opened O_APPEND. A partial write
        // would leave a truncated line, which `replay` reports rather than
        // skips -- see there for why silently dropping it is the wrong answer.
        f.write_all(&line)?;
        if entry.requires_durable_write() {
            f.sync_data()?;
            *recover(self.syncs.lock()) += 1;
        }
        Ok(())
    }

    fn replay(&self) -> io::Result<Vec<JournalEntry>> {
        let lines: Vec<String> = BufReader::new(File::open(&self.path)?)
            .lines()
            .collect::<io::Result<_>>()?;
        let last = lines.len().saturating_sub(1);
        let mut out = Vec::with_capacity(lines.len());
        for (n, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(e) => out.push(e),
                // A truncated FINAL line is the expected shape of a crash
                // during an append, and it is safe to drop: an entry that was
                // never `fsync`ed is by construction one the journal never
                // promised, and a ticket left non-terminal because of it is
                // closed by reconciliation. Refusing to start over it would
                // turn a routine crash into an outage.
                Err(_) if n == last => break,
                // Anywhere else it means the file was rewritten or corrupted
                // behind us. Recovery decides what is still owed on-chain; it
                // must not guess from a file it cannot trust.
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("journal line {} of {} is not a valid entry: {e}", n + 1, lines.len()),
                    ))
                }
            }
        }
        Ok(out)
    }

    fn sync_count(&self) -> u64 {
        *recover(self.syncs.lock())
    }
}
