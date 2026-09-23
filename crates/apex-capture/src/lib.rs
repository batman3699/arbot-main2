//! Capture Assurance Controller (Blueprint §2.4-2.9, §29.4-29.6, §46.1, §57).
//!
//! §16.1: this is a core dependency, not a later enhancement. It owns the
//! invariant that no admitted ticket can disappear silently (INV-01), and it is
//! built in shadow, before any live dispatch exists in the v4 path.

pub mod clock;
pub mod journal;
pub mod registry;

pub use clock::{Clock, ManualClock, SystemClock};
pub use journal::{FileJournal, InMemoryJournal, Journal, JournalEntry};
pub use registry::{RegistryError, RegistryMetrics, TicketGuard, TicketRegistry};
