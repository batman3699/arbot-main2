//! Task 8.5 R32.1 — the canary's send route: Base's sequencer first, BlockPI
//! when the sequencer has not taken it. `apex send-probe` measured BlockPI's
//! protected endpoint holding sends 0.6-1.7 s (median 877 ms to inclusion)
//! against the sequencer's 70-254 ms (median 145).

use alloy_primitives::{keccak256, B256};
use apex_chain::base::send::{FallbackTransport, RawTransport, SendAnswer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Answers `answer` (an acceptance names the transaction's own hash) and
/// counts its calls.
struct Scripted {
    answer: SendAnswer,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(answer: SendAnswer) -> Arc<Self> {
        Arc::new(Self { answer, calls: AtomicUsize::new(0) })
    }
}

#[async_trait::async_trait]
impl RawTransport for Scripted {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match &self.answer {
            SendAnswer::Accepted(_) => SendAnswer::Accepted(keccak256(raw)),
            other => other.clone(),
        }
    }
}

const RAW: &[u8] = b"a signed transaction";

fn accepted() -> SendAnswer {
    SendAnswer::Accepted(B256::ZERO)
}

async fn route(primary: SendAnswer, fallback: SendAnswer) -> (SendAnswer, usize, usize) {
    let (p, f) = (Scripted::new(primary), Scripted::new(fallback));
    let t = FallbackTransport::new(Arc::clone(&p) as Arc<dyn RawTransport>, Arc::clone(&f) as Arc<dyn RawTransport>);
    let answer = t.send_raw(RAW).await;
    (answer, p.calls.load(Ordering::Relaxed), f.calls.load(Ordering::Relaxed))
}

/// The sequencer took it: BlockPI is never asked.
#[tokio::test]
async fn the_sequencers_acceptance_is_the_answer() {
    assert_eq!(route(accepted(), accepted()).await, (SendAnswer::Accepted(keccak256(RAW)), 1, 0));
}

/// The sequencer refused (nothing sent) or went silent (perhaps sent): the
/// same signed bytes go to BlockPI, which can only land them once. Either
/// acceptance is an acceptance.
#[tokio::test]
async fn whatever_the_sequencer_did_not_take_goes_to_blockpi() {
    for primary in [SendAnswer::Refused("-32000: rate limited".into()), SendAnswer::Silent("timeout".into())] {
        assert_eq!(route(primary.clone(), accepted()).await, (SendAnswer::Accepted(keccak256(RAW)), 1, 1), "{primary:?}");
    }
}

/// Refused is a promise that nothing was sent: only both refusing keeps it.
/// One silence among them may have sent it.
#[tokio::test]
async fn refused_only_when_both_refused() {
    let no = || SendAnswer::Refused("no".into());
    let quiet = || SendAnswer::Silent("timeout".into());
    assert!(matches!(route(no(), no()).await.0, SendAnswer::Refused(_)));
    assert!(matches!(route(no(), quiet()).await.0, SendAnswer::Silent(_)));
    assert!(matches!(route(quiet(), no()).await.0, SendAnswer::Silent(_)));
    assert!(matches!(route(quiet(), quiet()).await.0, SendAnswer::Silent(_)));
}
