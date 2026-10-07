# Spike-first capture (Task 8.5 R21)

Approved by the operator 2026-10-08.

## Goal

Make the capture path useful in the volatile minutes where Base's arbitrage
value actually arrives, rather than in the calm hours it was tuned on.

## Evidence

Measured 2026-10-07 from chain data and the shadow's own journal:

- **The money is in spikes.** Inside our pool set, all 42 competing contracts
  together earned $306 in 23 calm hours and $5,559 in one spike hour. Over 7
  days, 65% of the value came from 33 trades in 9 ten-minute episodes.
- **The plane queues during a spike.** At 10:07 UTC the shadow produced 65
  candidates in blocks 52289155–60, the blocks where winners took two-hop
  Uniswap v3 gaps for $16–38. The plane handled them one at a time, in the
  search's fee order. Tickets 54–56 (worth $0.03–0.07) took 230–430 ms each in
  Tier 2 before ticket 57 (worth $17.98) was simulated, about 1.2 s after the
  first. It failed on its minimum output. Later tickets waited past their 2 s
  deadline, and 9 were simulated as `Expired`. 50 proposals reached refine
  after the book had moved, and found no profitable size.
- **Sizing ignores the lender.** The 02:07 UTC spike's trades were 30–84 WETH.
  Balancer held 28.5 WETH. Sizing is bounded only by the paying pool's holding
  (`LiveCycle::max_input`), and the risk gate then refuses any loan above the
  lender's holding. So an 84 WETH optimum is refused whole, rather than traded
  at 28.5 WETH.

## What changes

### 1. Proposals carry the search's net

`RouteProposal` gains `net_hint: Option<U256>`: Engine C's net at the size it
found, which `SizedOpportunity::net` already holds. Other engines leave it
`None`. It is an ordering input only, exactly as `size_hint` is: the economics
re-evaluates every proposal against the book as it stands.

### 2. The plane exposes proposing and handling separately

- `Plane::propose(&self, event) -> Result<Vec<RouteProposal>, Handled>`: the
  redelivery check, the candidate-generation budget, then the search. The
  `Err` carries the `Handled` that `on_event` returns today for those cases.
- `Plane::handle_proposal(&self, event, proposal) -> Handled`: today's private
  `handle`.
- `Plane::decline_stale(&self, proposal, age) -> Handled`: files the miss as
  `Decline::StaleState`, which already maps to `MissReason::StaleState`.

`on_event` becomes `propose` then `handle_proposal` for each, with unchanged
behaviour, so every existing plane test still describes it.

### 3. A queue in the shadow's capture loop

A new `shadow::queue::PendingQueue` holds `(Arc<StateEvent>, RouteProposal)`
entries:

- `merge(event, proposals)`: a proposal for a route already queued replaces
  it, keyed on `route.route_hash`, because the newer one was priced against
  newer state.
- `pop_best(now) -> Popped { stale, best }`: first removes every entry whose
  `found_at` is more than `MAX_PROPOSAL_AGE` (1 s, half the 2 s dispatch
  deadline) before `now`, then takes the entry with the highest `net_hint`.
  `None` ranks lowest. Ties go to the newest `found_at`, then the lowest route
  hash, so the order is deterministic.
- `drop_conflicting(pools) -> usize`: removes entries whose route shares a
  pool with a ticket just dispatched; they would race our own trade.
- `max_depth()`: the deepest the queue has been.

The capture loop becomes:

1. If the queue is empty, wait for an event.
2. Take every event already waiting, without blocking (`try_recv`). Each is
   counted and checked against the book's status as today, then proposed and
   merged.
3. If the book is not `Verified`, clear the queue: its entries were priced
   against a book being rebuilt.
4. `pop_best`: file each stale entry through `decline_stale`, then handle the
   best entry.
5. If that ticket was dispatched, `drop_conflicting` its pools.

New events are merged before every pick, so a valuable candidate never waits
behind cheaper ones, and the queue drains old work by dropping it rather than
simulating it.

### 4. Sizing within the lender's holding

`LivePricer`, which serves both the search and the economics, takes a
`watch::Receiver<Option<U256>>` of the lender's holding. For a cycle that
borrows, the largest input it may price is the smaller of the paying pool's
holding and the lender's. `None` means zero, the same fail-closed rule an
unread pool balance follows. The shadow's head loop publishes the reader
view's `lender_holding` after each refresh, as R18's block context is
published.

### 5. Reporting

The report gains `queue: { stale, conflicts, max_depth }`, and
`shadow-status.sh` prints them.

## What does not change

- Tier 2, the risk gate and last-mile revalidation. Last-mile check 9 still
  compares the loan with the lender's holding.
- One ticket at a time. Parallel Tier 2 for candidates on separate pools is
  deferred by operator decision: revisit if a spike loses such a candidate to
  waiting.
- `on_event`, which the tests drive.

## Verification

1. **Queue unit tests:** ordering by net; a newer proposal replacing an older
   one; stale entries dropped exactly at the 1 s boundary; conflicting entries
   dropped; deterministic ties.
2. **Plane:** every existing `on_event` test passes unchanged; a test drives
   `propose` and `handle_proposal` directly.
3. **Lender cap:** a cycle whose unbounded optimum exceeds the lender's
   holding is sized at or below it; an unknown holding gives no size.
4. **Mutation checks** on the queue's ordering, staleness and conflict rules,
   and on the cap.
5. **All five CI gates** and the clean-worktree check of each commit.
6. **Live:** after the restart the report shows the queue counters. In the
   next burst, the journal shows tickets handled in descending expected net,
   and no `Expired` simulation failures.

## Risks

- A proposal for a route whose pools did not move is dropped after 1 s. It is
  proposed again by the next event that moves its pools. A gap no event
  touches again is lost, which is rare: gaps open on swaps.
- In a long spike, low-value proposals are dropped unsimulated. That is the
  intent.
- Draining every waiting event runs the search for each, which takes
  milliseconds per event.

## Rollout

Tests first. Then mutation checks, all five CI gates and the clean-worktree
check of each commit. This ships with R22, the second Slipstream factory, in
one release build and one restart, which resets the 14-day clock.
