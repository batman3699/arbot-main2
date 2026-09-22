#!/usr/bin/env bash
# INV-11 / Blueprint §5.3: "No global mutable state is used as shared truth
# between search workers. Workers receive immutable snapshots or versioned read
# handles."
#
# The shape this forbids is the one the legacy fast path actually uses:
#   Published<T> = Arc<StdMutex<Option<Arc<T>>>>     (base_fast.rs:2054)
# A mutex on the fast path is both the shared mutable truth §5.3 rules out and a
# §2.4 capture hazard -- a reader can block behind a writer while a ticket's
# deadline runs down. apex-state::Versioned<T> replaces it with ArcSwap.
#
# Scoped to the NEW crates. arb-exec-legacy keeps the old shape until its
# consumers are ported in Phase 2; gating it now would be red on arrival.
set -euo pipefail
cd "$(dirname "$0")/../.."

pattern='(Mutex|RwLock)<[[:space:]]*Option[[:space:]]*<[[:space:]]*Arc'
# Skip comments: versioned.rs documents the forbidden shape in order to explain
# what it replaces, and a guard that cannot tell code from prose about code
# punishes writing the explanation down.
hits=$(grep -rEn "$pattern" crates/apex-*/src 2>/dev/null \
       | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|/\*|\*)' || true)

if [ -n "$hits" ]; then
  echo "INV-11 VIOLATION: shared mutable state used as published truth." >&2
  echo "$hits" >&2
  echo >&2
  echo "Use apex_state::Versioned<T>: wait-free reads, and every snapshot" >&2
  echo "carries the StateVersion and ReconstructionStatus a worker needs to" >&2
  echo "say WHICH state it is holding." >&2
  exit 1
fi
echo "ok: no Mutex/RwLock<Option<Arc<..>>> published state in apex-* crates"
