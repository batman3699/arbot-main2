# R28 Live Canary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The reactive lane sends real trades, one at a time, with a priority fee of a fifth of each trade's expected net. Every loss is bounded, every trade is reconciled to the wei, and the armed lane stays in shadow.

**Architecture:**

- **A new command.** `apex canary` runs the shadow's process with `DispatchLane::Canary` in place of the null dispatcher.
- **A new module, `apex_runtime::canary`, holds:**
  - the ledger (write-ahead, fsynced);
  - the limits and halts;
  - the sender;
  - the settlement-and-reconciliation chain;
  - boot recovery.
- **The plane gains:**
  - an async `Sender` port;
  - the canary lane and its INV-17 waiver;
  - the one-wei floor;
  - the tip in the fee caps;
  - a funding check;
  - correct nonce handling on refusals and on-chain reverts.
- **Submission is a separate type in `apex-chain`,** next to the read-only transport, never inside it.

**Tech Stack:** Rust (tokio, async-trait, reqwest 0.11, serde/serde_json, alloy-primitives 0.8, ethers-core/ethers-signers 2.0.14), anvil/cast for the fork proof, bash for ops scripts.

**Spec:** `docs/superpowers/specs/2026-10-15-live-canary-design.md` (commits 598d53a, c1aca8e). Read it before any task. The plan argues from it.

## Global Constraints

**Repository rules:**
- Never run `cargo fmt`. Match the surrounding code's style by hand.
- Use `-j 3` on every cargo command.
- Never `git add -A`. Stage named paths only, then check that `git diff --stat -- <paths>` is empty.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

**Secrets:**
- Never print, echo or log a secret: `BLOCKPI_KEY`, `PRIVATE_KEY`, `TRADER_PRIVATE_KEY`, or any `APEX_SECRET_*` value.
- Endpoint URLs carry the key in the path. Hold them as `Secret`s and name an endpoint by `endpoint_label` only.
- Do not modify `.env`.
- No new non-secret environment variables (env_coverage, blueprint §2.4). Configuration goes in YAML.

**Never broadcast a mainnet transaction.**
- Owner transactions are prepared and dry-run only; the operator broadcasts them.
- The fork proof runs against a local anvil.

**Never point a network test at the shadow's BlockPI key.** A burst of reads hit the account's rate limit on 2026-10-14. The fork proof forks from `https://mainnet.base.org`.

**Values, from the spec:**
- Loss cap 0.001 ETH (`1000000000000000` wei); trader reserve 0.0001 ETH (`100000000000000`).
- 5 reverts in a row, 2 refusals in a row, 100 landed trades.
- Receipts polled every 250 ms, settlement timeout 10,000 ms.
- `tip_share_bps: 2000`; the canary keeps `max_cost_confidence_bps: 1000`.
- `profit_recipient` `0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3`.
- The canary journal `var/apex/canary.journal`, ledger `var/apex/canary.jsonl`, stop file `var/apex/canary.stop`.

**Addresses:**

| Role | Address |
|---|---|
| Executor clone | `0x8940B565D050974b2b589B70De43bCb753b1F93B` |
| BatchRouter (the executor's owner) | `0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2` |
| Owner | `0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3` |
| Trader | `0xCB436Ba3acb945b3fc8EE6345857262584356595` |
| WETH | `0x4200000000000000000000000000000000000006` |

**Selectors and topics:**

| Name | Value |
|---|---|
| `executors(address)` | `0x9ac2a011` |
| `isCircuitOpen()` | `0x1cb3bb96` |
| `profitRecipient()` | `0x7e2bad1e` |
| `feeRecipient()` | `0x46904840` |
| `ProfitRealised(address,uint256,uint256)` topic | `0x230bad4a4399a7f96d7dc55ada0f3e590e98017ef0fb0ef2ae55914350e5d832` |
| `Transfer` topic | `0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef` |

**Gates, all five, before every commit:**
1. `scripts/ci/*.sh`;
2. clippy `-D warnings` over CI's crates, `--all-targets`;
3. `cargo test --workspace --all-targets`;
4. `cargo test --workspace --doc`;
5. `forge test`.

`check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` are known red before this work; every other gate must pass.

## Review Focus

These failure modes are implied by the spec but no task's main tests cover them. Each line has a test added to its owning task.

1. **An endpoint answers a send with a hash that is not our transaction's,** or with something that is not a hash. Expected: never "accepted". It is `Silent`, which halts, because something may have been sent (Task 6, `a_send_answered_with_another_hash_is_silent`).
2. **A receipt with no `l1Fee`,** from a node or format change. Expected: the trade is uninterpretable, never reconciled at a zero L1 cost (Task 7, `a_receipt_without_an_l1_fee_is_not_reconciled`).
3. **A ledger whose last line was cut short by a crash mid-write.** Expected: the cut line is ignored, because nothing is sent until the append returns. A bad line anywhere else refuses to open (Task 5, `a_torn_last_line_is_ignored_and_a_torn_middle_line_refuses`).
4. **The trader's pending-nonce view lagging our own send by a block.** Expected: the tripwire does not fire on `now < boot + sent`, only on `now > boot + sent` (Task 9, `the_canary_tripwire_fires_only_beyond_our_sends`).
5. **The stop file appearing between the risk gate and the send.** Expected: the sender refuses and releases the nonce, and the refusal does not count toward `max_consecutive_refusals` (Task 6, `a_hold_at_the_sender_is_not_an_endpoint_refusal`).

---

## File map

**Create:**

| File | What it holds |
|---|---|
| `crates/apex-chain/src/base/send.rs` | `RawTransport`, `SendAnswer`, `HttpSubmitter`: the one type in `apex-chain` that sends |
| `crates/apex-runtime/src/canary/mod.rs` | module doc, re-exports |
| `crates/apex-runtime/src/canary/ledger.rs` | `Record`, `Sent`, `Outcome`, `Settled`, `Halt`, `Ledger` (append + fsync, replay) |
| `crates/apex-runtime/src/canary/limits.rs` | `Limits`, `Totals`, `State`, `Canary` (implements `risk::Hold`), `CanaryReport` |
| `crates/apex-runtime/src/canary/send.rs` | `CanaryDispatcher` (implements `plane::Sender`), `InFlight`, `Pending` |
| `crates/apex-runtime/src/canary/chain.rs` | `CanaryChain` (implements `SettlementFeed` and `live::adapter::Attribution`), `read_receipt` |
| `crates/apex-runtime/src/canary/recover.rs` | `CanaryRecovery` (implements `ChainOutcomeSource`) |
| `crates/apex-runtime/tests/canary_limits.rs` | Task 5 |
| `crates/apex-runtime/tests/canary_send.rs` | Task 6 |
| `crates/apex-runtime/tests/canary_chain.rs` | Tasks 7 and 8 |
| `crates/apex-runtime/tests/canary_fork.rs` | Task 11; ignored, network |
| `ops/canary.base.yaml` | the canary's configuration |
| `ops/host/apex-canary.service` | the canary's systemd unit |
| `scripts/canary-reset.sh` | clears a halt, with the service stopped |

**Modify:**

| File | Change |
|---|---|
| `crates/apex-runtime/src/plane.rs` | `Sender`, `SendContext`, the canary lane, `Admission::CanaryOnly`, INV-17 lane check, floor, `fee_caps`, funding, nonce handling, `Decline::SettlementReverted` |
| `crates/apex-runtime/src/risk.rs` | the canary waiver, `Hold` |
| `crates/apex-runtime/src/econ.rs` | `tip_for`, `with_tip_share`, the tip in `assemble` |
| `crates/apex-runtime/src/live/abi.rs` | four selectors |
| `crates/apex-runtime/src/live/reader.rs` | the executor's switches in the view |
| `crates/apex-runtime/src/live/adapter.rs` | `Attribution`, `with_attribution` |
| `crates/apex-runtime/src/shadow/config.rs` | `tip_share_bps`, `CanaryConfig`, `load_canary` |
| `crates/apex-runtime/src/shadow/mod.rs` | canary wiring, tripwire, report block |
| `crates/apex-runtime/src/lib.rs` | `pub mod canary;` |
| `crates/apex-runtime/src/main.rs` | `apex canary` |
| `crates/apex-capture/src/signer/pool.rs` | `LaneAssignment::gas_reserve_wei` |
| `crates/apex-chain/src/adapter.rs` | `AdapterError::Reverted` |
| `crates/apex-chain/src/base/mod.rs` | `pub mod send;` |
| `crates/apex-chain/src/rpc/mod.rs` | `debug_traceTransaction` on the read allowlist |
| `ops/shadow.base.yaml` | `tip_share_bps` |
| `ops/host/README.md` | the canary section |
| `scripts/shadow-status.sh` | the canary line |
| `PLAN.md` | the R28 entry |
| tests | `shadow.rs`, `end_to_end.rs`, `one_call.rs`, `local_signer.rs`, `live_risk.rs`, `live_econ.rs`, `live_reader.rs`, `shadow_parts.rs`, `support/mod.rs` |

---

### Task 1: The canary lane, its INV-17 waiver, and an async sender

**Files:**
- Modify: `crates/apex-runtime/src/plane.rs` (imports at lines 80–110; `DispatchLane`/`LaneKind`/`Admission` at 388–446; the INV-17 backstop and dispatch inside `protocol`, 1132–1192)
- Modify: `crates/apex-runtime/src/risk.rs` (`LiveRiskGate` struct and `new`, 55–80; `admit`, 104–163)
- Modify: `crates/apex-runtime/tests/shadow.rs:213`, `crates/apex-runtime/tests/end_to_end.rs:514-516`
- Test: `crates/apex-runtime/tests/live_risk.rs`, `crates/apex-runtime/tests/shadow.rs`

**Interfaces:**
- Produces, in `apex_runtime::plane`:
  - `pub struct SendContext { pub chain: ChainId, pub strategy: StrategyId, pub route_hash: B256, pub venues: Vec<VenueId>, pub gas_limit: GasLimit, pub fees: FeeCaps, pub expected_gross_wei: U256, pub expected_net_wei: i128, pub waived: Vec<String> }`
  - `#[async_trait] pub trait Sender: Send + Sync { async fn send(&self, req: &DispatchRequest, permit: &DispatchPermit<'_>, now: UnixNanos, context: &SendContext) -> Result<AckLadder, DispatchError>; }`, with a blanket impl for every `Dispatcher + Send + Sync + ?Sized`
  - `DispatchLane::{Shadow(Arc<NullDispatcher>), Live(Arc<dyn Sender>), Canary(Arc<dyn Sender>)}`
  - `LaneKind::{Shadow, Live, Canary}`
  - `Admission::CanaryOnly { waived: Vec<Clause> }`
- Produces, in `apex_runtime::risk`:
  - `pub trait Hold: Send + Sync { fn reason(&self) -> Option<String>; }`
  - `LiveRiskGate::with_hold(self, Arc<dyn Hold>) -> Self`

- [ ] **Step 1: Write the failing gate tests** in `crates/apex-runtime/tests/live_risk.rs`, after `a_shadow_lane_waives_nothing_else`:

```rust
/// **R28's waiver, at the gate.** On a canary lane a heuristic route is admitted
/// `CanaryOnly`, naming INV-17's clause; a proven one waives nothing; a live lane
/// still refuses the heuristic route.
#[test]
fn a_canary_lane_waives_route_authorization_and_names_it() {
    let gate = healthy_gate();
    let mut c = admissible();
    c.certificate_status = CertificateStatus::Heuristic;
    assert_eq!(
        gate.admit(&c, &good_sim(&c), LaneKind::Canary),
        Ok(Admission::CanaryOnly { waived: vec![Clause::RouteAuthorizationValid] })
    );
    assert!(gate.admit(&c, &good_sim(&c), LaneKind::Live).is_err(), "a live lane still refuses it");
    let proven = admissible();
    assert_eq!(gate.admit(&proven, &good_sim(&proven), LaneKind::Canary), Ok(Admission::Full));
}

/// Every other clause refuses on a canary lane exactly as on a live one.
#[test]
fn a_canary_lane_waives_nothing_else() {
    let gate = healthy_gate();
    let mut c = admissible();
    c.certificate_status = CertificateStatus::Heuristic;
    c.robust_ev = 0;
    let err = gate.admit(&c, &good_sim(&c), LaneKind::Canary).expect_err("must refuse");
    let Decline::RiskRefused { rule } = err else { panic!("{err:?}") };
    assert!(rule.starts_with(Clause::ExpectedNetEvPositive.label()), "refused as: {rule}");
}

struct Held(&'static str);

impl apex_runtime::risk::Hold for Held {
    fn reason(&self) -> Option<String> {
        Some(self.0.to_string())
    }
}

/// **A hold stops every candidate, with its own reason, on every lane.**
#[test]
fn a_hold_refuses_with_its_reason() {
    let gate = healthy_gate().with_hold(std::sync::Arc::new(Held("canary halted: loss cap")));
    let c = admissible();
    for lane in [LaneKind::Shadow, LaneKind::Live, LaneKind::Canary] {
        assert_eq!(
            gate.admit(&c, &good_sim(&c), lane),
            Err(Decline::RiskRefused { rule: "canary halted: loss cap".into() }),
            "{lane:?}"
        );
    }
}
```

- [ ] **Step 2: Run them and watch them fail to compile**

Run: `cargo test -j 3 -p apex-runtime --test live_risk 2>&1 | tail -20`
Expected: errors that `LaneKind::Canary`, `Admission::CanaryOnly` and `with_hold` do not exist.

- [ ] **Step 3: Implement the lane, the admission and the sender** in `plane.rs`.

Imports: change `use apex_capture::dispatch::{DispatchError, DispatchRequest, Dispatcher, NullDispatcher};` to `use apex_capture::dispatch::{AckLadder, DispatchError, DispatchRequest, Dispatcher, NullDispatcher};`; add `DispatchPermit` to the `apex_capture::recover` import; change `use apex_types::ids::{ChainId, SubmissionLaneId, TicketId};` to `use apex_types::ids::{ChainId, StrategyId, SubmissionLaneId, TicketId, VenueId};`.

Replace the `DispatchLane` enum and its `impl` with:

```rust
#[derive(Clone)]
pub enum DispatchLane {
    /// §16.1's null dispatcher. Records; sends nothing.
    Shadow(Arc<NullDispatcher>),
    /// Anything that sends. Only an [`Admission::Full`] reaches it.
    Live(Arc<dyn Sender>),
    /// R28's canary: sends, and an [`Admission::CanaryOnly`] reaches it as well
    /// as a full one. INV-17's route clause is waived on it and named on every
    /// admission and send record (spec §1a, operator decision 2026-10-15).
    Canary(Arc<dyn Sender>),
}

impl DispatchLane {
    pub const fn kind(&self) -> LaneKind {
        match self {
            Self::Shadow(_) => LaneKind::Shadow,
            Self::Live(_) => LaneKind::Live,
            Self::Canary(_) => LaneKind::Canary,
        }
    }
}

/// What the risk gate is told about where its admission would lead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneKind {
    Shadow,
    Live,
    /// R28: a lane that sends, on which INV-17's route clause alone is waived.
    Canary,
}

/// The risk gate's yes, and how much of a yes it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Every clause held. May reach any lane.
    Full,
    /// Every clause held except `waived`, which only a shadow plane may waive.
    /// **Reaches only [`DispatchLane::Shadow`].**
    ShadowOnly { waived: Vec<Clause> },
    /// Every clause held except `waived`, waived for the canary (R28).
    /// **Reaches only [`DispatchLane::Canary`].**
    CanaryOnly { waived: Vec<Clause> },
}

impl Admission {
    /// The clauses this admission did not satisfy. Empty for `Full`.
    pub fn waived(&self) -> &[Clause] {
        match self {
            Self::Full => &[],
            Self::ShadowOnly { waived } | Self::CanaryOnly { waived } => waived,
        }
    }

    /// Whether this admission may reach a lane of `lane`'s kind (INV-17).
    pub const fn fits(&self, lane: LaneKind) -> bool {
        match self {
            Self::Full => true,
            Self::ShadowOnly { .. } => matches!(lane, LaneKind::Shadow),
            Self::CanaryOnly { .. } => matches!(lane, LaneKind::Canary),
        }
    }
}
```

After `FeeCaps`, add:

```rust
/// What the plane knows about a ticket that its signed bytes do not say, handed
/// to a sending lane with them (R28). The canary writes it to its ledger before
/// the transaction leaves, and reconciles the receipt against it.
#[derive(Clone, Debug, PartialEq)]
pub struct SendContext {
    pub chain: ChainId,
    pub strategy: StrategyId,
    pub route_hash: B256,
    pub venues: Vec<VenueId>,
    pub gas_limit: GasLimit,
    pub fees: FeeCaps,
    /// What the cycle returns above what it borrowed, in wei of the loan token.
    pub expected_gross_wei: U256,
    pub expected_net_wei: i128,
    /// The clauses the admission waived, by label. Empty on a live lane.
    pub waived: Vec<String>,
}

/// A dispatcher that may wait on the network (R28).
///
/// `apex_capture::dispatch::Dispatcher` is synchronous: a transport handed bytes.
/// Sending to an endpoint is I/O, and a blocking call on the capture path would
/// stall the runtime's worker. So the lanes that send hold this, and every
/// synchronous dispatcher is one through the blanket impl below, which is how
/// the lifecycle tests' dispatchers keep driving the live protocol unchanged.
#[async_trait::async_trait]
pub trait Sender: Send + Sync {
    async fn send(
        &self,
        req: &DispatchRequest,
        permit: &DispatchPermit<'_>,
        now: UnixNanos,
        context: &SendContext,
    ) -> Result<AckLadder, DispatchError>;
}

#[async_trait::async_trait]
impl<D: Dispatcher + Send + Sync + ?Sized> Sender for D {
    async fn send(
        &self,
        req: &DispatchRequest,
        permit: &DispatchPermit<'_>,
        now: UnixNanos,
        _context: &SendContext,
    ) -> Result<AckLadder, DispatchError> {
        self.dispatch(req, permit, now)
    }
}
```

In `protocol`, replace the INV-17 backstop (`if let (DispatchLane::Live(_), Admission::ShadowOnly { waived }) = …`) with:

```rust
        // INV-17, structurally. A waiver reaches only the lane it was granted
        // for, **whatever the gate that granted it was told**: the gate is one
        // component and this is the plane that owns the lane. Refused before
        // `Authorized`, so it is a refusal rather than an admitted ticket that
        // later failed.
        let lane_kind = self.ports.dispatch.kind();
        if !admission.fits(lane_kind) {
            return Err(Decline::RiskRefused {
                rule: format!(
                    "INV-17: {} was waived for another lane, and this plane's lane is {lane_kind:?}",
                    labels(admission.waived()).join(", ")
                ),
            });
        }
```

Make step 5 keep the caps it signs with. Change `let payload = self.ports.signer.sign(&authorization, &call, gas_limit, fee_caps(&readings))?;` to:

```rust
        let fees = fee_caps(&readings);
        let payload = self.ports.signer.sign(&authorization, &call, gas_limit, fees)?;
```

Replace the dispatch `match`:

```rust
        let dispatched_at = self.ports.registry.now();
        let context = SendContext {
            chain: candidate.chain_id,
            strategy: candidate.strategy,
            route_hash: candidate.route.route_hash,
            venues: candidate.venue_set.clone(),
            gas_limit,
            fees,
            expected_gross_wei: candidate.gross_profit,
            expected_net_wei: candidate.expected_net_profit,
            waived: labels(admission.waived()),
        };
        let dispatched = match &self.ports.dispatch {
            DispatchLane::Shadow(null) => null.dispatch(&request, &permit, dispatched_at),
            DispatchLane::Live(sender) | DispatchLane::Canary(sender) => {
                sender.send(&request, &permit, dispatched_at, &context).await
            }
        };
```

- [ ] **Step 4: Implement the gate's canary waiver and `Hold`** in `risk.rs`.

Add after the imports:

```rust
/// What may stop every candidate for a reason outside the risk crate (R28): the
/// canary's halts and its stop file. Read on every admission, like the posture,
/// because it is a fact about the system and not about the candidate.
pub trait Hold: Send + Sync {
    fn reason(&self) -> Option<String>;
}
```

Add a field `hold: Option<std::sync::Arc<dyn Hold>>` to `LiveRiskGate`, set it to `None` in `new`, and add:

```rust
    /// R28: refuse every candidate while `hold` gives a reason.
    #[must_use]
    pub fn with_hold(mut self, hold: std::sync::Arc<dyn Hold>) -> Self {
        self.hold = Some(hold);
        self
    }
```

In `admit`, directly after the posture check:

```rust
        // ---- 1a. A hold: the canary's halt or stop file. Also a fact about
        // the system, so it answers before any clause.
        if let Some(reason) = self.hold.as_ref().and_then(|h| h.reason()) {
            return Err(Decline::RiskRefused { rule: reason });
        }
```

Replace the `waived` computation and the final `match`:

```rust
        let waived = match lane {
            LaneKind::Shadow | LaneKind::Canary if !ctx.route_authorization_valid => {
                ctx.route_authorization_valid = true;
                vec![Clause::RouteAuthorizationValid]
            }
            LaneKind::Shadow | LaneKind::Live | LaneKind::Canary => Vec::new(),
        };

        match EligibilityGate::evaluate(&ctx, &self.policy) {
            Decision::Admit if waived.is_empty() => Ok(Admission::Full),
            Decision::Admit if lane == LaneKind::Canary => Ok(Admission::CanaryOnly { waived }),
            Decision::Admit => Ok(Admission::ShadowOnly { waived }),
            Decision::Reject { clause } => Err(Decline::RiskRefused {
                rule: format!("{} ({})", clause.label(), clause.source()),
            }),
        }
```

Extend the comment above the waiver to say the canary gets the same waiver, by operator decision on 2026-10-15 (spec §1a).

- [ ] **Step 5: Fix the two tests that cast to the old trait object**

In `tests/shadow.rs:213`: `dispatch: DispatchLane::Live(Arc::clone(sender) as Arc<dyn Dispatcher + Send + Sync>),` becomes `dispatch: DispatchLane::Live(Arc::clone(sender) as Arc<dyn apex_runtime::plane::Sender>),`.

In `tests/end_to_end.rs:514-516`: `Arc::clone(&dispatcher) as Arc<dyn apex_capture::dispatch::Dispatcher + Send + Sync>,` becomes `Arc::clone(&dispatcher) as Arc<dyn apex_runtime::plane::Sender>,`.

Remove a `Dispatcher` import that becomes unused.

- [ ] **Step 6: Write the plane-level tests** in `tests/shadow.rs`, after `a_shadow_waiver_cannot_reach_a_live_lane`. First, beside `Rig::live`:

```rust
    fn canary(sender: Arc<dyn apex_runtime::plane::Sender>) -> Self {
        let null = Arc::new(NullDispatcher::new());
        Self { dispatch: DispatchLane::Canary(sender), ..Self::shadow(&null) }
    }
```

Then, beside `RogueGate`, a gate that grants a canary waiver anywhere, and a sender that keeps each context:

```rust
/// Grants a canary-only admission whatever lane it is told.
struct RogueCanaryGate;

impl RiskGate for RogueCanaryGate {
    fn admit(&self, _c: &Candidate, _s: &SimulationResult, _l: LaneKind) -> Result<Admission, Decline> {
        Ok(Admission::CanaryOnly { waived: vec![Clause::RouteAuthorizationValid] })
    }
}

/// Sends nothing and keeps every context it was handed.
#[derive(Default)]
struct KeepsContext(std::sync::Mutex<Vec<apex_runtime::plane::SendContext>>);

#[async_trait::async_trait]
impl apex_runtime::plane::Sender for KeepsContext {
    async fn send(
        &self,
        _req: &DispatchRequest,
        _permit: &DispatchPermit<'_>,
        now: UnixNanos,
        context: &apex_runtime::plane::SendContext,
    ) -> Result<AckLadder, DispatchError> {
        self.0.lock().unwrap().push(context.clone());
        let mut ladder = AckLadder::new(now);
        let _ = ladder.observe(LifecycleStage::TransportAccepted, now);
        Ok(ladder)
    }
}

/// **The decision, as a test.** On a canary plane a heuristic route is sent,
/// and the sender is told which clause was waived.
#[tokio::test]
async fn a_heuristic_route_is_sent_on_a_canary_lane_with_its_waiver() {
    let sender = Arc::new(KeepsContext::default());
    let rig = Rig::canary(Arc::clone(&sender) as Arc<dyn apex_runtime::plane::Sender>);
    let plane = rig.plane();
    let _ = one_outcome(&plane, 0).await;
    let seen = sender.0.lock().unwrap();
    assert_eq!(seen.len(), 1, "sent once");
    assert_eq!(seen[0].waived, vec![Clause::RouteAuthorizationValid.label().to_string()]);
    assert_eq!(seen[0].expected_net_wei, rig.candidate.expected_net_profit);
}

/// A canary waiver granted on a live plane is refused by the plane itself.
#[tokio::test]
async fn a_canary_waiver_cannot_reach_a_live_lane() {
    let sender = Arc::new(SendingDispatcher::default());
    let rig = Rig { risk: Arc::new(RogueCanaryGate), ..Rig::live(&sender) };
    let rule = refused_by(&one_outcome(&rig.plane(), 0).await);
    assert!(rule.contains("INV-17"), "refused for: {rule}");
    assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
}

/// And a shadow waiver cannot reach a canary lane.
#[tokio::test]
async fn a_shadow_waiver_cannot_reach_a_canary_lane() {
    let sender = Arc::new(KeepsContext::default());
    let rig = Rig { risk: Arc::new(RogueGate), ..Rig::canary(Arc::clone(&sender) as Arc<dyn apex_runtime::plane::Sender>) };
    let rule = refused_by(&one_outcome(&rig.plane(), 0).await);
    assert!(rule.contains("INV-17"), "refused for: {rule}");
    assert!(sender.0.lock().unwrap().is_empty());
}
```

Add `DispatchPermit`, `AckLadder` and `async_trait` to the imports as needed.

- [ ] **Step 7: Run the touched tests and see them pass**

Run: `cargo test -j 3 -p apex-runtime --test live_risk --test shadow --test end_to_end --test real_commitments --test real_search 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: every `test result: ok`.

- [ ] **Step 8: Mutation check.** Apply each mutation, run `cargo test -j 3 -p apex-runtime --test live_risk --test shadow --no-fail-fast`, confirm a failure, restore with `git checkout -- <file>`, then `touch` the file:
  - `fits`: `Self::CanaryOnly { .. } => true`;
  - risk: `LaneKind::Shadow if` without `| LaneKind::Canary`;
  - risk: delete the hold check.

  Each must be caught.

- [ ] **Step 9: Commit**

```bash
git add crates/apex-runtime/src/plane.rs crates/apex-runtime/src/risk.rs crates/apex-runtime/tests/live_risk.rs crates/apex-runtime/tests/shadow.rs crates/apex-runtime/tests/end_to_end.rs
git commit -m "feat(plane): a canary lane, its INV-17 waiver, and a sender that can wait (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: The plane's money rules: floor, tip caps, funding, nonces, reverts

**Files:**
- Modify: `crates/apex-runtime/src/plane.rs` (`lock` at 899–913; `fee_caps` at 502–511; `protocol` steps 2, 4–6, 10; `Decline`, its `Display`, `miss_reason`; `terminal_for`)
- Modify: `crates/apex-capture/src/signer/pool.rs` (`impl LaneAssignment`, near 301)
- Modify: `crates/apex-chain/src/adapter.rs` (`AdapterError`, 165–183)
- Modify: `crates/apex-runtime/tests/support/mod.rs` (`FakeChain`, 478–566)
- Test: `crates/apex-runtime/tests/end_to_end.rs`, `crates/apex-runtime/tests/one_call.rs`, `crates/apex-runtime/tests/local_signer.rs`

**Interfaces:**
- Consumes: Task 1's `SendContext`, `DispatchLane::Canary`, `LaneKind`.
- Produces:
  - `pub fn fee_caps(r: &LiveReadings, c: &TotalExecutionCost) -> FeeCaps`;
  - `Decline::SettlementReverted { class: RevertClass, gas_used: u64, spent_wei: u128 }`;
  - `AdapterError::Reverted { tx: B256, class: RevertClass, gas_used: u64, spent_wei: u128 }`;
  - `LaneAssignment::gas_reserve_wei(&self) -> u128`;
  - `FakeChain::reverting(self) -> Self`.

- [ ] **Step 1: Write the failing tests** in `tests/end_to_end.rs`, after `an_identical_commitment_cannot_be_locked_twice`:

```rust
/// **R28: the on-chain floor is one wei.** Once sent, a trade that lands with
/// any positive gross loses less than one that reverts after both swaps, so the
/// commitment's minimum is the loan plus one wei, whatever the expected net.
#[tokio::test]
async fn the_floor_is_one_wei_whatever_the_expected_net() {
    let plane = landing_plane();
    let c = candidate(1, 47_079_437, 320_000_000_000);
    let locked = plane.lock(&c).expect("locks");
    assert_eq!(locked.commitment().min_profit, U256::from(1u64));
}
```

In `tests/one_call.rs`, replace the body of the test that calls `fee_caps(&r)` (line ~204):

```rust
    let mut cost = candidate(1, 47_079_437, 320_000_000_000).total_execution_cost;
    cost.priority_fee = 0;
    assert_eq!(fee_caps(&r, &cost), FeeCaps { max_fee_per_gas: 400_000, max_priority_fee_per_gas: 0 });
```

Keep the ceiling assertion, passing `&cost`. Then add:

```rust
/// **R28: the tip is signed per unit of gas.** The economics charges the tip as
/// a total; the caps divide it by the expected gas, rounding down, and add it
/// to twice the base fee under the ceiling.
#[test]
fn the_tip_is_signed_per_unit_of_expected_gas() {
    let r = readings(); // observed fee 200,000, ceiling 5,000,000
    let mut cost = candidate(1, 47_079_437, 320_000_000_000).total_execution_cost;
    cost.gas_used_distribution.p50 = apex_types::cost::GasUsed(400_000);
    cost.priority_fee = 4_000_000_399; // 10,000.000998 a gas
    let caps = fee_caps(&r, &cost);
    assert_eq!(caps.max_priority_fee_per_gas, 10_000);
    assert_eq!(caps.max_fee_per_gas, 410_000);
    // Under a ceiling that binds, the tip never exceeds the cap.
    let mut low = r.clone();
    low.fee_ceiling_wei = 5_000;
    let capped = fee_caps(&low, &cost);
    assert_eq!(capped, FeeCaps { max_fee_per_gas: 5_000, max_priority_fee_per_gas: 5_000 });
}
```

Update `tests/local_signer.rs:255` to `fee_caps(&r, &candidate(1, 47_079_437, 320_000_000_000).total_execution_cost)` if its fixture's tip is zero. Otherwise use the test's own cost value.

Add to `tests/end_to_end.rs`. Note: `Refuses` returns a rejection, and `pool()` hands out lane 1:

```rust
/// Refuses every payload, as an endpoint that answered no.
struct Refuses;

impl apex_capture::dispatch::Dispatcher for Refuses {
    fn dispatch(
        &self,
        _req: &apex_capture::dispatch::DispatchRequest,
        _permit: &apex_capture::recover::DispatchPermit<'_>,
        _now: UnixNanos,
    ) -> Result<apex_capture::dispatch::AckLadder, apex_capture::dispatch::DispatchError> {
        Err(apex_capture::dispatch::DispatchError::Rejected { lane: SubmissionLaneId(1), detail: "no".into() })
    }
}

/// **R28: a refused send gives its nonce back.** The endpoint said nothing
/// reached the chain; keeping the nonce reserved would make the next ticket
/// sign behind a gap that never closes.
#[tokio::test]
async fn a_refused_send_releases_its_nonce() {
    let pool = Arc::new(pool());
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(Box::new(InMemoryJournal::new()), Box::new(ManualClock::at(BOOT.0)))),
        pool: Arc::clone(&pool),
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Live(Arc::new(Refuses)),
        chain: Arc::new(FakeChain::landing()),
        search: Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        econ: Arc::new(PassThroughEconomics::default()),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        calls: Arc::new(FixtureCalls),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");
    let _ = plane.on_event(&recorded_stream()[0]).await;
    let need = apex_capture::signer::LaneRequirements {
        chain: pool.auth().chain,
        executor: pool.auth().executor,
        executor_version: pool.auth().executor_version,
        min_gas_reserve_wei: 0,
    };
    let lane = pool.assign(&need).expect("the lane is free again");
    assert_eq!(lane.reserve_nonce(0, BOOT).get(), 0, "the refused send's nonce is reused");
}

/// **R28: a transaction that reverted on chain closes as a revert**, with its
/// cost, and its nonce is spent: a later ticket signs the next one.
#[tokio::test]
async fn an_onchain_revert_closes_as_a_revert_and_spends_its_nonce() {
    let pool = Arc::new(pool());
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(Box::new(InMemoryJournal::new()), Box::new(ManualClock::at(BOOT.0)))),
        pool: Arc::clone(&pool),
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Live(Arc::new(NullDispatcher::new())),
        chain: Arc::new(FakeChain::landing().reverting()),
        search: Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        econ: Arc::new(PassThroughEconomics::default()),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        calls: Arc::new(FixtureCalls),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");
    let handled = plane.on_event(&recorded_stream()[0]).await;
    let [Handled::Closed { outcome, .. }] = &handled[..] else { panic!("{handled:?}") };
    let TicketOutcome::ExplicitFailure { code: TerminalFailure::Reverted { revert_class, .. }, .. } = &**outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(*revert_class, apex_types::sim::RevertClass::MinOutNotMet);
    let need = apex_capture::signer::LaneRequirements {
        chain: pool.auth().chain,
        executor: pool.auth().executor,
        executor_version: pool.auth().executor_version,
        min_gas_reserve_wei: 0,
    };
    assert_eq!(pool.assign(&need).unwrap().reserve_nonce(0, BOOT).get(), 1, "nonce 0 was spent");
}

/// **R28: a sending lane checks the signer can pay.** Gas limit times the max
/// fee, plus the lane's reserve, against the balance the reader saw; a shadow
/// lane, whose trader holds nothing, does not.
#[tokio::test]
async fn a_sending_lane_refuses_a_signer_that_cannot_pay() {
    let mut poor = readings();
    poor.signer_balance_wei = 1_000;
    let make = |dispatch: DispatchLane| {
        Plane::new(Ports {
            registry: Arc::new(TicketRegistry::new(Box::new(InMemoryJournal::new()), Box::new(ManualClock::at(BOOT.0)))),
            pool: Arc::new(pool()),
            gate: Arc::new(DispatchGate::shut()),
            dispatch,
            chain: Arc::new(FakeChain::landing()),
            search: Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
            econ: Arc::new(PassThroughEconomics::default()),
            sim: Arc::new(AlwaysSucceeds),
            risk: Arc::new(AlwaysAdmits),
            commitments: Arc::new(FixtureCommitments),
            calls: Arc::new(FixtureCalls),
            signer: Arc::new(EchoSigner),
            live: Arc::new(FixedReadings(poor.clone())),
            settlement: Arc::new(LandsAndFinalizes),
        })
    };
    let live = make(DispatchLane::Live(Arc::new(NullDispatcher::new())));
    live.boot(&NoChain, BOOT).expect("boot");
    let handled = live.on_event(&recorded_stream()[0]).await;
    let [Handled::Closed { outcome, .. }] = &handled[..] else { panic!("{handled:?}") };
    assert!(format!("{outcome:?}").contains("signer_balance"), "{outcome:?}");

    let shadow = make(DispatchLane::Shadow(Arc::new(NullDispatcher::new())));
    shadow.boot(&NoChain, BOOT).expect("boot");
    let handled = shadow.on_event(&recorded_stream()[0]).await;
    let [Handled::Closed { outcome, .. }] = &handled[..] else { panic!("{handled:?}") };
    assert!(!format!("{outcome:?}").contains("signer_balance"), "the shadow does not check: {outcome:?}");
}
```

Add `FakeChain::reverting` to `tests/support/mod.rs`. Add a field `pub reverts: bool` to `FakeChain`, `reverts: false` in `landing()`, and:

```rust
    /// Every transaction reconciles as a revert at hop 2's minimum (R28).
    pub fn reverting(mut self) -> Self {
        self.reverts = true;
        self
    }
```

At the top of `reconcile_final_state`:

```rust
        if self.reverts {
            return Err(apex_chain::adapter::AdapterError::Reverted {
                tx: _h,
                class: apex_types::sim::RevertClass::MinOutNotMet,
                gas_used: 411_945,
                spent_wei: 2_471_670_000_000,
            });
        }
```

If any test builds `FakeChain { .. }` literally, run `grep -rn "FakeChain {" crates/apex-runtime/tests` and add `reverts: false` there.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-runtime --test end_to_end --test one_call 2>&1 | tail -30`
Expected: compile errors for `fee_caps` taking two arguments, `AdapterError::Reverted`, and `FakeChain::reverting`.

- [ ] **Step 3: Implement.**

`crates/apex-chain/src/adapter.rs`, add to `AdapterError` and its `Display`:

```rust
    /// The transaction was included and reverted (R28): a cost with a cause,
    /// not an unreachable chain.
    Reverted { tx: B256, class: apex_types::sim::RevertClass, gas_used: u64, spent_wei: u128 },
```

```rust
            Self::Reverted { tx, class, gas_used, spent_wei } => {
                write!(f, "{tx} reverted ({class:?}) after {gas_used} gas, costing {spent_wei} wei")
            }
```

`crates/apex-capture/src/signer/pool.rs`, in `impl LaneAssignment`:

```rust
    /// The lane's configured gas reserve, in wei (§18.2): what it must keep
    /// beyond the transaction it is about to sign (R28).
    pub fn gas_reserve_wei(&self) -> u128 {
        self.pool.lanes[self.index].cfg.gas_reserve_wei
    }
```

`plane.rs`, `lock`: replace the two-line `min_profit` computation with:

```rust
        // R28: one wei. Once sent, a trade that lands with any positive gross
        // loses less than one that reverts after both swaps, which on Base pays
        // its gas. The decision to send is where the expected net is held to
        // the policy's minimum (risk gate; last-mile check 6).
        let min_profit = U256::from(1u64);
```

Delete the now-unused expected-net conversion. Update `lock`'s doc paragraph if it says the floor is the net.

`plane.rs`, `fee_caps`:

```rust
/// The caps a ticket signs with, from the readings taken for its last-mile
/// check and the costs its economics charged.
///
/// Twice the base fee observed at the head, plus the tip — the conventional
/// EIP-1559 headroom, so a transaction survives a run of full blocks — and never
/// above the policy ceiling, which revalidation checks the observed fee against.
///
/// **The tip** is the economics' `priority_fee` (R28: a share of the expected
/// net), signed per unit of the expected gas, rounded down, and never above the
/// max fee, which EIP-1559 requires.
pub fn fee_caps(r: &LiveReadings, c: &TotalExecutionCost) -> FeeCaps {
    let gas = u128::from(c.gas_used_distribution.p50.0).max(1);
    let tip = c.priority_fee / gas;
    let max_fee_per_gas = r.observed_fee_wei.saturating_mul(2).saturating_add(tip).min(r.fee_ceiling_wei);
    FeeCaps { max_fee_per_gas, max_priority_fee_per_gas: tip.min(max_fee_per_gas) }
}
```

`plane.rs`, `protocol`: compute the caps before last-mile, and require the funding on a sending lane. Before `let ticket = ticket_snapshot(guard, &repriced);` in step 4:

```rust
        let fees = fee_caps(&readings, &repriced.total_execution_cost);
        // R28: a lane that sends must hold the most this transaction can cost,
        // plus its reserve. A shadow lane's trader holds nothing, and a ticket
        // refused for that would measure the wallet, not the trade.
        let required_reserve_wei = match self.ports.dispatch.kind() {
            LaneKind::Shadow => 0,
            LaneKind::Live | LaneKind::Canary => u128::from(gas_limit.0)
                .saturating_mul(fees.max_fee_per_gas)
                .saturating_add(assignment.gas_reserve_wei()),
        };
```

Add a `required_reserve_wei: u128` parameter to `last_mile_context` and use it in place of the literal `required_reserve_wei: 0`.

**One departure from the spec (§5), on purpose.** The spec also sets `LaneRequirements::min_gas_reserve_wei` to the lane's reserve. That field is compared with the lane's *configured* reserve (`SignerPool::assign`), not with any balance, so setting it to the lane's own reserve checks nothing. Leave it at 0. The funding check is last-mile check 7, against the balance the reader saw. Say so in the PLAN.md entry. Delete the Task 1 line `let fees = fee_caps(&readings);` from step 5; the `fees` computed above is the one signed with.

`plane.rs`, step 6: replace `let mut ladder = dispatched.map_err(Decline::DispatchFailed)?;` with:

```rust
        let mut ladder = match dispatched {
            Ok(l) => l,
            Err(e) => {
                // R28: an endpoint that answered no sent nothing, so the nonce
                // goes back; one that did not answer may have sent it, and the
                // nonce stays spent until the chain says otherwise.
                if matches!(e, DispatchError::Rejected { .. }) {
                    let _ = assignment.release_nonce(nonce, false);
                }
                self.ports.pool.record_outcome(assignment.lane(), false);
                return Err(Decline::DispatchFailed(e));
            }
        };
```

`plane.rs`, step 10: replace the `reconcile_final_state(...).await.map_err(...)?;` with:

```rust
        let realized = match self.ports.chain.reconcile_final_state(payload.hash).await {
            Ok(r) => r,
            // R28: included and reverted. The nonce is spent; the ticket closes
            // as the revert it is, with what it cost.
            Err(apex_chain::adapter::AdapterError::Reverted { class, gas_used, spent_wei, .. }) => {
                let _ = assignment.release_nonce(nonce, true);
                self.ports.pool.record_outcome(assignment.lane(), false);
                return Err(Decline::SettlementReverted { class, gas_used, spent_wei });
            }
            Err(e) => return Err(Decline::ChainUnavailable { detail: e.to_string() }),
        };
```

`plane.rs`, `Decline`: add the variant, its `Display` and its bucket:

```rust
    /// R28: the transaction was included and reverted on chain.
    SettlementReverted { class: RevertClass, gas_used: u64, spent_wei: u128 },
```

```rust
            Self::SettlementReverted { class, gas_used, spent_wei } => {
                write!(f, "reverted on chain ({class:?}) after {gas_used} gas, costing {spent_wei} wei")
            }
```

In `miss_reason`: `Self::SettlementReverted { class: RevertClass::Expired, .. } => MissReason::TooSlow,` and `Self::SettlementReverted { .. } => MissReason::CompetitorWon,`. A late revert means the state moved under the trade; a deadline means it arrived late.

In `terminal_for`: `Decline::SettlementReverted { class, .. } => TerminalFailure::Reverted { revert_class: *class, data: Vec::new() },`.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -j 3 -p apex-runtime --test end_to_end --test one_call --test local_signer --test shadow --test real_commitments 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: all ok. Then: `cargo test -j 3 -p apex-capture -p apex-chain 2>&1 | grep -E "test result|FAILED"`, all ok.

- [ ] **Step 5: Run `scripts/ci/every_rejection_explains.sh`.** It must pass with the new `Decline` variant.

- [ ] **Step 6: Mutation check.** Apply each, confirm the named test fails, restore and `touch`:
  - `min_profit = U256::from(2u64)`;
  - `tip / gas` → `tip`;
  - `LaneKind::Live | LaneKind::Canary =>` → `0`;
  - delete the release on `Rejected`;
  - `release_nonce(nonce, true)` → `false` in step 10.

- [ ] **Step 7: Commit**

```bash
git add crates/apex-runtime/src/plane.rs crates/apex-capture/src/signer/pool.rs crates/apex-chain/src/adapter.rs crates/apex-runtime/tests/support/mod.rs crates/apex-runtime/tests/end_to_end.rs crates/apex-runtime/tests/one_call.rs crates/apex-runtime/tests/local_signer.rs
git commit -m "feat(plane): one-wei floor, the tip in the caps, funding on sending lanes, nonces that follow the chain (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: A priority fee of a fifth of the expected net

**Files:**
- Modify: `crates/apex-runtime/src/econ.rs` (struct 270–281, `new` 284–303, `with_flash` 323, `assemble` 528–620)
- Modify: `crates/apex-runtime/src/shadow/config.rs` (`Policy`, 162–177; validation in `from_yaml_str`)
- Modify: `crates/apex-runtime/src/shadow/mod.rs:544-549` (`with_tip_share`)
- Modify: `ops/shadow.base.yaml`
- Test: `crates/apex-runtime/tests/live_econ.rs`, `crates/apex-runtime/tests/shadow_parts.rs`

**Interfaces:**
- Produces:
  - `pub fn tip_for(net: i128, share_bps: u32) -> u128` in `apex_runtime::econ`;
  - `LiveEconomics::with_tip_share(self, share_bps: u32) -> Self`;
  - `Policy::tip_share_bps: u32`.
- `assemble` sets `total_execution_cost.priority_fee` to the tip, and `expected_net_profit = gross − cost`, with the tip in the cost.

- [ ] **Step 1: Write the failing tests** in `tests/live_econ.rs`, after `the_gross_is_what_the_cycle_returns_above_what_it_took`:

```rust
/// **R28: the tip is a share of the expected net, and a loss pays none.**
#[test]
fn the_tip_is_a_share_of_the_net() {
    use apex_runtime::econ::tip_for;
    assert_eq!(tip_for(1_000_000, 2_000), 200_000);
    assert_eq!(tip_for(9_999, 2_000), 1_999, "rounded down");
    assert_eq!(tip_for(0, 2_000), 0);
    assert_eq!(tip_for(-5, 2_000), 0, "a loss pays no tip");
    assert_eq!(tip_for(i128::MAX, 10_000), i128::MAX as u128, "no overflow at the extremes");
}

/// **R28: the economics charges the tip, and the size does not move.** The tip
/// scales the net, so the size that maximizes the net before it maximizes the
/// net after.
#[tokio::test]
async fn the_tip_is_charged_and_the_size_is_unchanged() {
    let plain = econ_over(200, L1FeeModel::unvalidated());
    let tipped = econ_over(200, L1FeeModel::unvalidated()).with_tip_share(2_000);
    let p = proposal(2);
    let r0 = apex_runtime::workers::refine_concurrently(&plain, &p).await.expect("refines");
    let r1 = apex_runtime::workers::refine_concurrently(&tipped, &p).await.expect("refines");
    assert_eq!(r0.input_amount, r1.input_amount, "the tip does not move the size");
    let c0 = plain.assemble(&p, r0).expect("assembles");
    let c1 = tipped.assemble(&p, r1).expect("assembles");
    assert_eq!(c0.total_execution_cost.priority_fee, 0);
    let tip = apex_runtime::econ::tip_for(c0.expected_net_profit, 2_000);
    assert!(tip > 0, "the fixture pays");
    assert_eq!(c1.total_execution_cost.priority_fee, tip);
    assert_eq!(c1.expected_net_profit, c0.expected_net_profit - i128::try_from(tip).unwrap());
}
```

In `tests/shadow_parts.rs`, add `  tip_share_bps: 2000` under `max_cost_confidence_bps: 10000` in `yaml()`. Add to `the_shipped_config_parses`:

```rust
    // R28: a fifth of the expected net, bid as the priority fee.
    assert_eq!(c.policy.tip_share_bps, 2_000);
```

And a new test:

```rust
/// R28: a tip share above the whole net is refused.
#[test]
fn a_tip_share_above_the_net_is_refused() {
    let var = "APEX_SECRET_SHADOW_TEST_TIP";
    let text = yaml(var, "var/apex/shadow.journal").replace("tip_share_bps: 2000", "tip_share_bps: 10001");
    assert!(matches!(ShadowConfig::from_yaml_str(&text, &env(var, SECRET)), Err(ShadowConfigError::Invalid(_))));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-runtime --test live_econ --test shadow_parts 2>&1 | tail -20`
Expected: compile errors for `tip_for`, `with_tip_share` and `tip_share_bps`.

- [ ] **Step 3: Implement.**

`econ.rs`, near `cost_i128`:

```rust
/// R28: the priority fee, as a share of the expected net, in basis points. A
/// net at or below zero pays nothing. Split so the product cannot overflow.
pub fn tip_for(net: i128, share_bps: u32) -> u128 {
    let Ok(net) = u128::try_from(net) else { return 0 };
    let share = u128::from(share_bps.min(10_000));
    net / 10_000 * share + net % 10_000 * share / 10_000
}
```

Add the field `tip_share_bps: u32` to `LiveEconomics` (0 in `new`), and:

```rust
    /// R28: bid this share of each candidate's expected net, in bps, as its
    /// priority fee. The tip scales the net, so it does not move the size.
    #[must_use]
    pub fn with_tip_share(mut self, share_bps: u32) -> Self {
        self.tip_share_bps = share_bps;
        self
    }
```

`assemble`: replace the opening `let cost = cost_i128(&r.costs);` and the use of `r.costs` below it. Move the gross computation first, and keep its comment block:

```rust
    fn assemble(&self, p: &RouteProposal, r: Refinement) -> Result<Candidate, Decline> {
        // (the gross comment block, unchanged)
        let gross = i128::try_from(to_u128(r.expected_output))
            .unwrap_or(i128::MAX)
            .saturating_sub(i128::try_from(to_u128(r.input_amount.get())).unwrap_or(i128::MAX));
        // R28: the tip, a share of the net before it, charged as the priority
        // fee. A revert also pays the tip on the gas it used, and the failure
        // cost leaves that out: under 1% of the net at a 20% tip and a 5% prior.
        let mut costs = r.costs;
        costs.priority_fee = tip_for(gross.saturating_sub(cost_i128(&costs)), self.tip_share_bps);
        let cost = cost_i128(&costs);
```

Then, in the rest of `assemble`, use `costs` wherever `r.costs` appeared (the Tier 0 input `cost: costs.clone()`, and `total_execution_cost: costs`). Keep `expected_net_profit: gross.saturating_sub(cost)`.

`shadow/config.rs`, `Policy`:

```rust
    /// R28: the share of each candidate's expected net bid as its priority fee,
    /// in bps. In both the shadow's file and the canary's, so the two price the
    /// same trade the same way.
    pub tip_share_bps: u32,
```

In `from_yaml_str`, beside the period checks:

```rust
        if p.tip_share_bps > 10_000 {
            return Err(invalid("policy.tip_share_bps above 10000 bids more than the net"));
        }
```

`ops/shadow.base.yaml`, under `max_cost_confidence_bps: 10000`:

```yaml
  # R28: a fifth of each trade's expected net, bid as its priority fee. Rivals
  # tipped 0.004 gwei at the median and 0.40 at the 90th percentile (R25's
  # contest analysis); on a cents-sized trade this is ~0.005-0.01 gwei.
  tip_share_bps: 2000
```

`shadow/mod.rs:544`: `LiveEconomics::new(…).with_flash(terms)` becomes `LiveEconomics::new(…).with_tip_share(config.policy.tip_share_bps).with_flash(terms)`.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -j 3 -p apex-runtime --test live_econ --test shadow_parts --test live_costs --test live_pricing 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: all ok.

- [ ] **Step 5: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - `share_bps.min(10_000)` → `share_bps`, killed by the overflow assertion;
  - `costs.priority_fee = tip_for(…)` → `0`;
  - delete the config validation.

- [ ] **Step 6: Commit**

```bash
git add crates/apex-runtime/src/econ.rs crates/apex-runtime/src/shadow/config.rs crates/apex-runtime/src/shadow/mod.rs ops/shadow.base.yaml crates/apex-runtime/tests/live_econ.rs crates/apex-runtime/tests/shadow_parts.rs
git commit -m "feat(econ): a priority fee of a fifth of the expected net (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: The owner's on-chain switches stop sends before any gas

**Files:**
- Modify: `crates/apex-runtime/src/live/abi.rs` (`mod selector`, after `BALANCE_OF`)
- Modify: `crates/apex-runtime/src/live/reader.rs` (`ChainView` 51–65, `refresh` 111–156, `read` 174)
- Test: `crates/apex-runtime/tests/live_reader.rs`

**Interfaces:**
- Produces: `ChainView::executor_allows_signer: bool` and `ChainView::circuit_open: bool`; `selector::{EXECUTORS, IS_CIRCUIT_OPEN, PROFIT_RECIPIENT, FEE_RECIPIENT}`.
- `LiveReadings::executor_version` is the plan version only when the code matches, the executor allows the signer, and the circuit is closed. Otherwise it is 0, which last-mile check 2 refuses.

- [ ] **Step 1: Write the failing tests** in `tests/live_reader.rs`.

Give `Node` two switches: add `allowed: Mutex<bool>` and `open: Mutex<bool>` fields, `true` and `false` in `node()`. Add the encoder:

```rust
/// `aggregate3`'s `(bool success, bytes returnData)[]`, one 32-byte word each.
fn aggregate3_answers(words: &[U256]) -> String {
    let w = |v: U256| v.to_be_bytes::<32>().to_vec();
    let u = |v: usize| w(U256::from(v));
    let n = words.len();
    let mut out = [u(32), u(n)].concat();
    for i in 0..n {
        out.extend(u(n * 32 + i * 128));
    }
    for word in words {
        out.extend([u(1), u(64), u(32), w(*word)].concat());
    }
    format!("0x{}", hex::encode(out))
}
```

In `Node::call`, before the Morpho `eth_call` arm:

```rust
            // R28: the executor's two switches, `executors(signer)` and
            // `isCircuitOpen()`, in one multicall.
            "eth_call" if params.to_string().to_lowercase().contains("9ac2a011") => Ok(json!(aggregate3_answers(&[
                U256::from(u8::from(*self.allowed.lock().unwrap())),
                U256::from(u8::from(*self.open.lock().unwrap())),
            ]))),
```

Tests:

```rust
/// **R28: the owner's switches stop a ticket before it is signed.** An executor
/// that no longer allows the signer, or whose circuit is open, reads as version
/// 0, which last-mile check 2 refuses, so no gas is spent finding out.
#[tokio::test]
async fn the_executors_switches_read_as_no_executor() {
    for (allowed, open, expect_version) in [(true, false, true), (false, false, false), (true, true, false)] {
        let n = node();
        *n.allowed.lock().unwrap() = allowed;
        *n.open.lock().unwrap() = open;
        let reader = ChainReader::new(n.clone(), book(), config(Duration::from_secs(60)));
        reader.refresh(&head(100), 30_000_000).await.expect("refreshes");
        let v = reader.view().unwrap();
        assert_eq!((v.executor_allows_signer, v.circuit_open), (allowed, open));
        let r = reader.read(&ticket()).await.expect("reads");
        assert_eq!(r.executor_version != 0, expect_version, "allowed {allowed}, open {open}");
    }
}
```

`head(number)` and `ticket()` are the file's existing helpers (lines 134 and 139). Update the test that counts a view's RPC calls by one, because the switches are one more `eth_call`.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -j 3 -p apex-runtime --test live_reader 2>&1 | tail -20`
Expected: compile errors for the two fields.

- [ ] **Step 3: Implement.**

`abi.rs`, in `mod selector`:

```rust
    /// `executors(address)`: whether the executor lets an address trade (R28).
    pub const EXECUTORS: [u8; 4] = [0x9a, 0xc2, 0xa0, 0x11];
    /// `isCircuitOpen()`: the owner's on-chain stop (R28).
    pub const IS_CIRCUIT_OPEN: [u8; 4] = [0x1c, 0xb3, 0xbb, 0x96];
    /// `profitRecipient()` and `feeRecipient()`: where the executor pays (R28).
    pub const PROFIT_RECIPIENT: [u8; 4] = [0x7e, 0x2b, 0xad, 0x1e];
    pub const FEE_RECIPIENT: [u8; 4] = [0x46, 0x90, 0x48, 0x40];
```

`reader.rs`, `ChainView`:

```rust
    /// R28: `executors(signer)` and `isCircuitOpen()`, the owner's two stops.
    pub executor_allows_signer: bool,
    pub circuit_open: bool,
```

In `refresh`, add a fifth concurrent read to the `tokio::join!`:

```rust
        let switches = [
            (c.executor, abi::call_address(selector::EXECUTORS, c.signer)),
            (c.executor, abi::call0(selector::IS_CIRCUIT_OPEN)),
        ];
        let (code, balance, nonce, holding, switched) = tokio::join!(
            reads.code_hash(c.executor, head.number),
            self.rpc.call("eth_getBalance", json!([c.signer, at])),
            self.rpc.call("eth_getTransactionCount", json!([c.signer, "pending"])),
            reads.multicall(&lender, head.number),
            reads.multicall(&switches, head.number),
        );
        let switched = switched?;
        let flag = |i: usize, what: &str| {
            switched.get(i).cloned().flatten().and_then(|d| abi::word_uint(&d, 0, 8)).map(|v| v != 0)
                .ok_or_else(|| malformed(what))
        };
```

The `malformed` closure must be declared before `flag`; move it up. In the `ChainView` literal: `executor_allows_signer: flag(0, "executors(signer)")?, circuit_open: flag(1, "isCircuitOpen()")?,`.

In `read`, replace the version line:

```rust
            // Version 0, which check 2 refuses, unless this is the configured
            // code, it still lets the signer trade, and the owner has not
            // opened its circuit (R28).
            executor_version: if v.executor_code_hash == c.executor_code_hash && v.executor_allows_signer && !v.circuit_open {
                c.plan_version
            } else {
                0
            },
```

Check for other `ChainView { … }` literals with `grep -rn "ChainView {" crates/apex-runtime` and add the two fields to each.

- [ ] **Step 4: Run it and see it pass**

Run: `cargo test -j 3 -p apex-runtime --test live_reader 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: ok.

- [ ] **Step 5: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - drop `&& v.executor_allows_signer`;
  - drop `&& !v.circuit_open`.

- [ ] **Step 6: Commit**

```bash
git add crates/apex-runtime/src/live/abi.rs crates/apex-runtime/src/live/reader.rs crates/apex-runtime/tests/live_reader.rs
git commit -m "feat(reader): the owner's on-chain stops refuse a ticket before it is signed (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: The canary's ledger, limits and halts

**Files:**
- Create: `crates/apex-runtime/src/canary/mod.rs`, `crates/apex-runtime/src/canary/ledger.rs`, `crates/apex-runtime/src/canary/limits.rs`
- Modify: `crates/apex-runtime/src/lib.rs` (`pub mod canary;`)
- Create: `scripts/canary-reset.sh`
- Test: `crates/apex-runtime/tests/canary_limits.rs`

**Interfaces:**
- Consumes: `risk::Hold` (Task 1).
- Produces:
  - `canary::ledger::{Record, Sent, Outcome, Settled, Halt, Ledger}`;
  - `canary::limits::{Limits, Totals, Canary, CanaryReport}`;
  - `Canary::open(ledger: &Path, stop_file: &Path, limits: Limits) -> io::Result<Canary>`;
  - `Canary::{sent, settle, halt}(…) -> io::Result<()>`;
  - `Canary::{halted, open_sends, sent_since_boot, report}`.

`Sent` and `Outcome`:

```rust
pub struct Sent {
    pub ticket: u64, pub nonce: u64, pub tx: B256, pub at_ms: u64,
    pub chain: ChainId, pub strategy: StrategyId, pub route_hash: B256, pub venues: Vec<VenueId>,
    pub gas_limit: u64, pub max_fee_per_gas: u128, pub max_priority_fee_per_gas: u128,
    pub expected_gross_wei: U256, pub expected_net_wei: i128, pub waived: Vec<String>,
}
pub struct Outcome { pub tx: B256, pub at_ms: u64, pub settled: Settled }
```

`Settled` is tagged `result`:
- `Landed { block, gas_used, effective_gas_price_wei, l1_fee_wei, gross_wei, cost_wei, net_wei }`
- `Reverted { block, gas_used, effective_gas_price_wei, l1_fee_wei, cost_wei, class, early }`
- `Refused { detail }`, `Silent { detail }`, `Unresolved { detail }`, `Unexplained { detail }`

`Halt` is tagged `halt`: `LossCap { lost_wei }`, `Reverts { in_a_row }`, `Refusals { in_a_row }`, `Unresolved { tx }`, `Unexplained { tx }`, `Tripwire { boot, now, sent }`, `Done { landed }`.

- [ ] **Step 1: Write the failing tests** in `crates/apex-runtime/tests/canary_limits.rs`:

```rust
//! Task 8.5 R28: the canary's ledger, its limits and its halts
//! (`apex_runtime::canary`).

use alloy_primitives::{B256, U256};
use apex_runtime::canary::ledger::{Halt, Outcome, Record, Sent, Settled};
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::risk::Hold;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use std::path::Path;

fn limits() -> Limits {
    Limits { loss_cap_wei: 1_000, max_consecutive_reverts: 3, max_consecutive_refusals: 2, max_landed: 4 }
}

fn sent(n: u8) -> Sent {
    Sent {
        ticket: u64::from(n),
        nonce: u64::from(n),
        tx: B256::repeat_byte(n),
        at_ms: 1,
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        route_hash: B256::repeat_byte(0x44),
        venues: vec![VenueId(1)],
        gas_limit: 500_000,
        max_fee_per_gas: 20_000_000,
        max_priority_fee_per_gas: 1_000_000,
        expected_gross_wei: U256::from(900u64),
        expected_net_wei: 400,
        waived: vec!["route_authorization_valid".into()],
    }
}

fn landed(n: u8, net: i128) -> Outcome {
    Outcome {
        tx: B256::repeat_byte(n),
        at_ms: 2,
        settled: Settled::Landed {
            block: 1,
            gas_used: 400_000,
            effective_gas_price_wei: 1,
            l1_fee_wei: 0,
            gross_wei: net + 400_000,
            cost_wei: 400_000,
            net_wei: net,
        },
    }
}

fn reverted(n: u8, cost: u128) -> Outcome {
    Outcome {
        tx: B256::repeat_byte(n),
        at_ms: 2,
        settled: Settled::Reverted {
            block: 1,
            gas_used: 400_000,
            effective_gas_price_wei: 1,
            l1_fee_wei: 0,
            cost_wei: cost,
            class: "MinOutNotMet".into(),
            early: false,
        },
    }
}

fn open(dir: &Path) -> Canary {
    Canary::open(&dir.join("canary.jsonl"), &dir.join("canary.stop"), limits()).expect("opens")
}

#[test]
fn the_loss_cap_trips_at_its_threshold_and_not_one_short() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(1)).unwrap();
    c.settle(reverted(1, 999)).unwrap();
    assert_eq!(c.halted(), None, "999 of 1,000 lost");
    c.sent(sent(2)).unwrap();
    c.settle(reverted(2, 1)).unwrap();
    assert_eq!(c.halted(), Some(Halt::LossCap { lost_wei: 1_000 }));
}

#[test]
fn wins_do_not_offset_the_loss_cap() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(1)).unwrap();
    c.settle(landed(1, 10_000)).unwrap();
    c.sent(sent(2)).unwrap();
    c.settle(landed(2, -1_000)).unwrap();
    assert_eq!(c.halted(), Some(Halt::LossCap { lost_wei: 1_000 }), "a 10,000 win does not cover a 1,000 loss");
}

#[test]
fn reverts_in_a_row_trip_and_a_landing_resets_the_count() {
    let d = tempfile::tempdir().unwrap();
    let c = Canary::open(&d.path().join("l"), &d.path().join("s"), Limits { loss_cap_wei: u128::MAX, ..limits() }).unwrap();
    for n in 1..=2 {
        c.sent(sent(n)).unwrap();
        c.settle(reverted(n, 1)).unwrap();
    }
    c.sent(sent(3)).unwrap();
    c.settle(landed(3, 5)).unwrap();
    for n in 4..=5 {
        c.sent(sent(n)).unwrap();
        c.settle(reverted(n, 1)).unwrap();
    }
    assert_eq!(c.halted(), None, "two in a row after the landing");
    c.sent(sent(6)).unwrap();
    c.settle(reverted(6, 1)).unwrap();
    assert_eq!(c.halted(), Some(Halt::Reverts { in_a_row: 3 }));
}

#[test]
fn refusals_in_a_row_trip() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    for n in 1..=2 {
        c.sent(sent(n)).unwrap();
        c.settle(Outcome { tx: B256::repeat_byte(n), at_ms: 2, settled: Settled::Refused { detail: "-32605".into() } }).unwrap();
    }
    assert_eq!(c.halted(), Some(Halt::Refusals { in_a_row: 2 }));
}

#[test]
fn a_silent_send_halts_as_unresolved_and_stays_open() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(1)).unwrap();
    c.settle(Outcome { tx: B256::repeat_byte(1), at_ms: 2, settled: Settled::Silent { detail: "timed out".into() } }).unwrap();
    assert_eq!(c.halted(), Some(Halt::Unresolved { tx: B256::repeat_byte(1) }));
    assert_eq!(c.open_sends().len(), 1, "recovery must look for it");
}

#[test]
fn the_canary_is_done_after_its_last_landing() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    for n in 1..=4 {
        c.sent(sent(n)).unwrap();
        c.settle(landed(n, 5)).unwrap();
    }
    assert_eq!(c.halted(), Some(Halt::Done { landed: 4 }));
}

#[test]
fn a_halt_survives_a_restart_and_a_reset_clears_it() {
    let d = tempfile::tempdir().unwrap();
    {
        let c = open(d.path());
        c.sent(sent(1)).unwrap();
        c.settle(reverted(1, 1_000)).unwrap();
    }
    let again = open(d.path());
    assert_eq!(again.halted(), Some(Halt::LossCap { lost_wei: 1_000 }));
    assert!(again.reason().unwrap().contains("loss cap"));
    drop(again);
    // What scripts/canary-reset.sh appends.
    std::fs::OpenOptions::new()
        .append(true)
        .open(d.path().join("canary.jsonl"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"kind\":\"reset\",\"at_ms\":3}\n"))
        .unwrap();
    let reset = open(d.path());
    assert_eq!(reset.halted(), None);
    assert_eq!(reset.report().totals.lost_wei, 0, "the cap counts from the reset");
}

#[test]
fn the_stop_file_holds_while_present_and_is_not_a_halt() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    assert_eq!(c.reason(), None);
    std::fs::write(d.path().join("canary.stop"), b"").unwrap();
    assert!(c.reason().unwrap().contains("stop file"));
    assert_eq!(c.halted(), None);
    std::fs::remove_file(d.path().join("canary.stop")).unwrap();
    assert_eq!(c.reason(), None);
}

#[test]
fn a_sent_record_is_on_disk_when_sent_returns() {
    let d = tempfile::tempdir().unwrap();
    let c = open(d.path());
    c.sent(sent(7)).unwrap();
    let text = std::fs::read_to_string(d.path().join("canary.jsonl")).unwrap();
    let r: Record = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(r, Record::Sent(sent(7)));
    assert_eq!(c.sent_since_boot(), 1);
}

/// Review focus 3: a crash mid-append leaves a torn last line, and nothing was
/// sent after it, because nothing is sent until the append returns. A torn line
/// anywhere else is damage, and the canary refuses to open over it.
#[test]
fn a_torn_last_line_is_ignored_and_a_torn_middle_line_refuses() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("canary.jsonl");
    let good = serde_json::to_string(&Record::Sent(sent(1))).unwrap();
    std::fs::write(&path, format!("{good}\n{{\"kind\":\"sen")).unwrap();
    assert_eq!(open(d.path()).open_sends().len(), 1);
    std::fs::write(&path, format!("{{\"kind\":\"sen\n{good}\n")).unwrap();
    assert!(Canary::open(&path, &d.path().join("s"), limits()).is_err());
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-runtime --test canary_limits 2>&1 | tail -5`
Expected: `unresolved import apex_runtime::canary`.

- [ ] **Step 3: Implement.** `crates/apex-runtime/src/canary/mod.rs`:

```rust
//! The live canary (Task 8.5 R28): the reactive lane's first real trades, one
//! at a time, with every loss bounded and every trade reconciled to the wei.
//!
//! | Module | Does |
//! |---|---|
//! | `ledger` | `canary.jsonl`: every send written and synced before it leaves, every outcome after |
//! | `limits` | the running totals, the halts, the stop file: [`limits::Canary`], the risk gate's [`crate::risk::Hold`] |
//! | `send` | [`send::CanaryDispatcher`]: the plane's `Sender` on the canary lane |
//! | `chain` | [`chain::CanaryChain`]: receipts, settlement and reconciliation |
//! | `recover` | [`recover::CanaryRecovery`]: what a crash left open, resolved at boot |
//!
//! The spec is `docs/superpowers/specs/2026-10-15-live-canary-design.md`.

pub mod ledger;
pub mod limits;

pub use ledger::{Halt, Outcome, Record, Sent, Settled};
pub use limits::{Canary, CanaryReport, Limits, Totals};
```

Later tasks add `pub mod send;`, `pub mod chain;` and `pub mod recover;`.

`crates/apex-runtime/src/canary/ledger.rs`:

```rust
//! `canary.jsonl`: one JSON object a line, append-only, every line synced.
//!
//! **Write-ahead.** A send record is on disk before the transaction leaves the
//! process, so a crash can never leave a sent transaction the canary does not
//! know about. A crash mid-append leaves a torn **last** line; nothing was sent
//! after it, so it is ignored. A torn line anywhere else is damage, and the
//! canary refuses to open over it.

use alloy_primitives::{B256, U256};
use apex_types::ids::{ChainId, StrategyId, VenueId};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Sent(Sent),
    Outcome(Outcome),
    Halted { at_ms: u64, halt: Halt },
    /// The operator's, with the service stopped (`scripts/canary-reset.sh`).
    Reset { at_ms: u64 },
}

/// Everything known about a transaction before it leaves: enough to reconcile
/// its receipt after a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub ticket: u64,
    pub nonce: u64,
    pub tx: B256,
    pub at_ms: u64,
    pub chain: ChainId,
    pub strategy: StrategyId,
    pub route_hash: B256,
    pub venues: Vec<VenueId>,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub expected_gross_wei: U256,
    pub expected_net_wei: i128,
    pub waived: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub tx: B256,
    pub at_ms: u64,
    pub settled: Settled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Settled {
    Landed {
        block: u64,
        gas_used: u64,
        effective_gas_price_wei: u128,
        l1_fee_wei: u128,
        gross_wei: i128,
        cost_wei: u128,
        net_wei: i128,
    },
    Reverted {
        block: u64,
        gas_used: u64,
        effective_gas_price_wei: u128,
        l1_fee_wei: u128,
        cost_wei: u128,
        class: String,
        /// Under 100,000 gas: the deadline, the commitment or the caller.
        early: bool,
    },
    /// The endpoint answered no. Nothing reached the chain.
    Refused { detail: String },
    /// No answer: it may or may not have left.
    Silent { detail: String },
    /// No receipt by the timeout.
    Unresolved { detail: String },
    /// Landed, and the executor's transfers and its event disagree.
    Unexplained { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "halt", rename_all = "snake_case")]
pub enum Halt {
    LossCap { lost_wei: u128 },
    Reverts { in_a_row: u32 },
    Refusals { in_a_row: u32 },
    Unresolved { tx: B256 },
    Unexplained { tx: B256 },
    Tripwire { boot: u64, now: u64, sent: u64 },
    Done { landed: u32 },
}

impl std::fmt::Display for Halt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LossCap { lost_wei } => write!(f, "loss cap: {lost_wei} wei lost on losing trades"),
            Self::Reverts { in_a_row } => write!(f, "{in_a_row} reverts in a row"),
            Self::Refusals { in_a_row } => write!(f, "{in_a_row} refused sends in a row"),
            Self::Unresolved { tx } => write!(f, "{tx} is unresolved"),
            Self::Unexplained { tx } => write!(f, "{tx} landed and its transfers disagree with its event"),
            Self::Tripwire { boot, now, sent } => {
                write!(f, "the trader's nonce is {now}, beyond {boot} at boot plus {sent} sends: the key is in use elsewhere")
            }
            Self::Done { landed } => write!(f, "done: {landed} trades landed"),
        }
    }
}

/// The open file, and every record it held when opened.
pub struct Ledger {
    file: Mutex<File>,
}

impl Ledger {
    pub fn open(path: &Path) -> io::Result<(Self, Vec<Record>)> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut records = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            match serde_json::from_str(line) {
                Ok(r) => records.push(r),
                // Torn by a crash mid-append: nothing was sent after it.
                Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => break,
                Err(e) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("canary ledger line {}: {e}", i + 1)))
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok((Self { file: Mutex::new(file) }, records))
    }

    /// Append one record and sync it. Returns only once it is on disk.
    pub fn append(&self, r: &Record) -> io::Result<()> {
        let mut line = serde_json::to_string(r).map_err(io::Error::other)?;
        line.push('\n');
        let mut f = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.write_all(line.as_bytes())?;
        f.sync_data()
    }
}
```

`crates/apex-runtime/src/canary/limits.rs`:

```rust
//! The canary's limits and halts (spec §8). Rebuilt from the ledger at boot,
//! so a halt survives a restart; cleared only by an operator's reset record.

use super::ledger::{Halt, Ledger, Outcome, Record, Sent, Settled};
use alloy_primitives::B256;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub loss_cap_wei: u128,
    pub max_consecutive_reverts: u32,
    pub max_consecutive_refusals: u32,
    pub max_landed: u32,
}

/// Since the last reset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub sent: u64,
    pub landed: u32,
    pub reverted_early: u32,
    pub reverted_late: u32,
    pub refused: u32,
    pub silent: u32,
    pub unresolved: u32,
    /// Losses on losing trades only: a revert's whole cost, a landing's net
    /// when below zero. Wins never offset it.
    pub lost_wei: u128,
    pub gross_wei: i128,
    pub cost_wei: u128,
    pub net_wei: i128,
    pub reverts_in_a_row: u32,
    pub refusals_in_a_row: u32,
}

#[derive(Debug, Default)]
struct State {
    totals: Totals,
    halted: Option<Halt>,
    open: BTreeMap<B256, Sent>,
}

impl State {
    /// Apply one record; the halt it trips, if it trips one now.
    fn apply(&mut self, r: &Record, limits: &Limits) -> Option<Halt> {
        let t = &mut self.totals;
        match r {
            Record::Sent(s) => {
                t.sent += 1;
                self.open.insert(s.tx, s.clone());
            }
            Record::Outcome(Outcome { tx, settled, .. }) => match settled {
                Settled::Landed { gross_wei, cost_wei, net_wei, .. } => {
                    self.open.remove(tx);
                    t.landed += 1;
                    t.gross_wei = t.gross_wei.saturating_add(*gross_wei);
                    t.cost_wei = t.cost_wei.saturating_add(*cost_wei);
                    t.net_wei = t.net_wei.saturating_add(*net_wei);
                    if *net_wei < 0 {
                        t.lost_wei = t.lost_wei.saturating_add(net_wei.unsigned_abs());
                    }
                    t.reverts_in_a_row = 0;
                    t.refusals_in_a_row = 0;
                }
                Settled::Reverted { cost_wei, early, .. } => {
                    self.open.remove(tx);
                    if *early { t.reverted_early += 1 } else { t.reverted_late += 1 }
                    t.cost_wei = t.cost_wei.saturating_add(*cost_wei);
                    t.net_wei = t.net_wei.saturating_sub(i128::try_from(*cost_wei).unwrap_or(i128::MAX));
                    t.lost_wei = t.lost_wei.saturating_add(*cost_wei);
                    t.reverts_in_a_row += 1;
                    t.refusals_in_a_row = 0;
                }
                Settled::Refused { .. } => {
                    self.open.remove(tx);
                    t.refused += 1;
                    t.refusals_in_a_row += 1;
                }
                // Both may have reached the chain: they stay open for recovery.
                Settled::Silent { .. } => {
                    t.silent += 1;
                    return self.trip(Halt::Unresolved { tx: *tx });
                }
                Settled::Unresolved { .. } => {
                    t.unresolved += 1;
                    return self.trip(Halt::Unresolved { tx: *tx });
                }
                Settled::Unexplained { .. } => {
                    self.open.remove(tx);
                    return self.trip(Halt::Unexplained { tx: *tx });
                }
            },
            Record::Halted { halt, .. } => {
                self.halted = Some(halt.clone());
                return None;
            }
            Record::Reset { .. } => {
                self.totals = Totals::default();
                self.halted = None;
                return None;
            }
        }
        let t = &self.totals;
        let tripped = if t.lost_wei >= limits.loss_cap_wei {
            Some(Halt::LossCap { lost_wei: t.lost_wei })
        } else if t.reverts_in_a_row >= limits.max_consecutive_reverts {
            Some(Halt::Reverts { in_a_row: t.reverts_in_a_row })
        } else if t.refusals_in_a_row >= limits.max_consecutive_refusals {
            Some(Halt::Refusals { in_a_row: t.refusals_in_a_row })
        } else if t.landed >= limits.max_landed {
            Some(Halt::Done { landed: t.landed })
        } else {
            None
        };
        tripped.and_then(|h| self.trip(h))
    }

    fn trip(&mut self, h: Halt) -> Option<Halt> {
        if self.halted.is_some() {
            return None;
        }
        self.halted = Some(h.clone());
        Some(h)
    }
}

/// What the five-minute report carries (spec §11).
#[derive(Clone, Debug, Serialize)]
pub struct CanaryReport {
    pub totals: Totals,
    pub halted: Option<String>,
    pub stop_file: bool,
    pub open: usize,
}

pub struct Canary {
    ledger: Ledger,
    limits: Limits,
    stop_file: PathBuf,
    state: Mutex<State>,
    sent_since_boot: AtomicU64,
}

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
        .unwrap_or(u64::MAX)
}

impl Canary {
    pub fn open(ledger: &Path, stop_file: &Path, limits: Limits) -> io::Result<Self> {
        let (ledger, records) = Ledger::open(ledger)?;
        let mut state = State::default();
        for r in &records {
            // A halt tripped while replaying was written when it tripped.
            let _ = state.apply(r, &limits);
        }
        Ok(Self {
            ledger,
            limits,
            stop_file: stop_file.to_path_buf(),
            state: Mutex::new(state),
            sent_since_boot: AtomicU64::new(0),
        })
    }

    fn record(&self, r: &Record) -> io::Result<()> {
        self.ledger.append(r)?;
        let tripped = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).apply(r, &self.limits);
        if let Some(halt) = tripped {
            tracing::error!(%halt, "the canary halted");
            self.ledger.append(&Record::Halted { at_ms: now_ms(), halt })?;
        }
        Ok(())
    }

    /// Written and synced before the transaction leaves the process.
    pub fn sent(&self, s: Sent) -> io::Result<()> {
        self.record(&Record::Sent(s))?;
        self.sent_since_boot.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn settle(&self, o: Outcome) -> io::Result<()> {
        self.record(&Record::Outcome(o))
    }

    /// Halt for a reason no record computes (the tripwire).
    pub fn halt(&self, halt: Halt) -> io::Result<()> {
        let fresh = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).trip(halt.clone());
        if fresh.is_some() {
            tracing::error!(%halt, "the canary halted");
            self.ledger.append(&Record::Halted { at_ms: now_ms(), halt })?;
        }
        Ok(())
    }

    pub fn halted(&self) -> Option<Halt> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).halted.clone()
    }

    /// Sends with no outcome: what boot recovery must resolve.
    pub fn open_sends(&self) -> Vec<Sent> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).open.values().cloned().collect()
    }

    pub fn sent_since_boot(&self) -> u64 {
        self.sent_since_boot.load(Ordering::Relaxed)
    }

    pub fn report(&self) -> CanaryReport {
        let s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        CanaryReport {
            totals: s.totals.clone(),
            halted: s.halted.as_ref().map(ToString::to_string),
            stop_file: self.stop_file.exists(),
            open: s.open.len(),
        }
    }
}

impl crate::risk::Hold for Canary {
    fn reason(&self) -> Option<String> {
        if let Some(h) = self.halted() {
            return Some(format!("canary halted: {h}"));
        }
        self.stop_file
            .exists()
            .then(|| format!("canary stop file {} is present", self.stop_file.display()))
    }
}
```

`lib.rs`: add `pub mod canary;` in alphabetical order among the modules.

`scripts/canary-reset.sh`:

```bash
#!/usr/bin/env bash
# Clear a canary halt (Task 8.5 R28). Run on the host with the service
# stopped: the running canary never re-reads its ledger, so a reset appended
# under it would take effect only at the next start, unannounced.
#   systemctl stop apex-canary && scripts/canary-reset.sh && systemctl start apex-canary
set -euo pipefail
ledger="${1:-var/apex/canary.jsonl}"
if systemctl is-active --quiet apex-canary 2>/dev/null; then
    echo "apex-canary is running: stop it first" >&2
    exit 1
fi
[ -f "$ledger" ] || { echo "no ledger at $ledger" >&2; exit 1; }
printf '{"kind":"reset","at_ms":%s}\n' "$(date +%s%3N)" >> "$ledger"
sync "$ledger"
echo "reset appended to $ledger; the cap counts from here"
```

Make it executable: `chmod +x scripts/canary-reset.sh`.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -j 3 -p apex-runtime --test canary_limits 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: ok.

- [ ] **Step 5: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - `t.lost_wei >= limits.loss_cap_wei` → `>`;
  - delete `t.reverts_in_a_row = 0;` in `Landed`;
  - `if *net_wei < 0` → `if *net_wei != 0`;
  - the torn-line arm without `!text.ends_with('\n')`;
  - `Reset` without `self.halted = None`.

- [ ] **Step 6: Commit**

```bash
git add crates/apex-runtime/src/canary/mod.rs crates/apex-runtime/src/canary/ledger.rs crates/apex-runtime/src/canary/limits.rs crates/apex-runtime/src/lib.rs scripts/canary-reset.sh crates/apex-runtime/tests/canary_limits.rs
git commit -m "feat(canary): the write-ahead ledger, its limits and halts (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: The sender

**Files:**
- Create: `crates/apex-chain/src/base/send.rs`
- Modify: `crates/apex-chain/src/base/mod.rs` (`pub mod send;`)
- Create: `crates/apex-runtime/src/canary/send.rs`
- Modify: `crates/apex-runtime/src/canary/mod.rs` (`pub mod send;`)
- Test: `crates/apex-chain/src/base/send.rs` (unit tests against a local socket), `crates/apex-runtime/tests/canary_send.rs`

**Interfaces:**
- Produces, in `apex_chain::base::send`:
  - `pub enum SendAnswer { Accepted(B256), Refused(String), Silent(String) }`;
  - `#[async_trait] pub trait RawTransport: Send + Sync { async fn send_raw(&self, raw: &[u8]) -> SendAnswer; }`;
  - `pub struct HttpSubmitter`, with `HttpSubmitter::new(url: &str, timeout: Duration) -> Result<Self, String>` and `label()`.
- Produces, in `apex_runtime::canary::send`:
  - `pub struct Pending { pub sent: Sent, pub receipt: Option<serde_json::Value> }`;
  - `pub struct InFlight(Mutex<BTreeMap<B256, Pending>>)`, with `insert`, `get`, `set_receipt`, `remove`;
  - `pub struct CanaryDispatcher`, with `CanaryDispatcher::new(canary: Arc<Canary>, transport: Arc<dyn RawTransport>, in_flight: Arc<InFlight>) -> Self` and `pub async fn send_raw(&self, ticket: u64, nonce: u64, raw: &[u8], ctx: &SendContext) -> Result<B256, DispatchError>`;
  - `impl Sender for CanaryDispatcher`.

- [ ] **Step 1: Write the failing tests.** In `crates/apex-runtime/tests/canary_send.rs`:

```rust
//! Task 8.5 R28: the canary's sender (`apex_runtime::canary::send`).

use alloy_primitives::{keccak256, B256, U256};
use apex_capture::dispatch::DispatchError;
use apex_chain::base::send::{RawTransport, SendAnswer};
use apex_runtime::canary::ledger::{Record, Settled};
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::canary::send::{CanaryDispatcher, InFlight};
use apex_runtime::plane::{FeeCaps, SendContext};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn ctx() -> SendContext {
    SendContext {
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        route_hash: B256::repeat_byte(0x44),
        venues: vec![VenueId(1)],
        gas_limit: GasLimit(500_000),
        fees: FeeCaps { max_fee_per_gas: 20_000_000, max_priority_fee_per_gas: 1_000_000 },
        expected_gross_wei: U256::from(900u64),
        expected_net_wei: 400,
        waived: vec!["route_authorization_valid".into()],
    }
}

/// Answers as scripted, and checks the ledger already holds the send record
/// when it is called: the write-ahead property, observed from the transport.
struct Scripted {
    answer: SendAnswer,
    ledger: PathBuf,
    saw_record: Mutex<Option<bool>>,
}

#[async_trait::async_trait]
impl RawTransport for Scripted {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer {
        let text = std::fs::read_to_string(&self.ledger).unwrap_or_default();
        let tx = format!("{:#x}", keccak256(raw));
        *self.saw_record.lock().unwrap() = Some(text.contains(&tx));
        match &self.answer {
            SendAnswer::Accepted(_) => SendAnswer::Accepted(keccak256(raw)),
            other => other.clone(),
        }
    }
}

fn rig(dir: &Path, answer: SendAnswer) -> (Arc<Canary>, Arc<Scripted>, CanaryDispatcher) {
    let ledger = dir.join("canary.jsonl");
    let canary = Arc::new(
        Canary::open(&ledger, &dir.join("canary.stop"), Limits { loss_cap_wei: 1_000, max_consecutive_reverts: 5, max_consecutive_refusals: 2, max_landed: 100 })
            .unwrap(),
    );
    let t = Arc::new(Scripted { answer, ledger, saw_record: Mutex::new(None) });
    let d = CanaryDispatcher::new(Arc::clone(&canary), Arc::clone(&t) as Arc<dyn RawTransport>, Arc::new(InFlight::default()));
    (canary, t, d)
}

#[tokio::test]
async fn the_send_record_is_on_disk_before_the_send_leaves() {
    let d = tempfile::tempdir().unwrap();
    let (_c, t, sender) = rig(d.path(), SendAnswer::Accepted(B256::ZERO));
    let tx = sender.send_raw(7, 3, b"raw-bytes", &ctx()).await.expect("accepted");
    assert_eq!(tx, keccak256(b"raw-bytes"));
    assert_eq!(*t.saw_record.lock().unwrap(), Some(true), "the transport saw the record already written");
}

#[tokio::test]
async fn a_refusal_is_rejected_counted_and_recorded() {
    let d = tempfile::tempdir().unwrap();
    let (c, _t, sender) = rig(d.path(), SendAnswer::Refused("-32605: flagged".into()));
    let err = sender.send_raw(7, 3, b"raw", &ctx()).await.expect_err("refused");
    assert!(matches!(err, DispatchError::Rejected { ref detail, .. } if detail.contains("-32605")), "{err:?}");
    assert_eq!(c.report().totals.refused, 1);
    assert_eq!(c.open_sends().len(), 0, "a refusal reached nothing");
}

#[tokio::test]
async fn silence_is_no_response_and_halts_as_unresolved() {
    let d = tempfile::tempdir().unwrap();
    let (c, _t, sender) = rig(d.path(), SendAnswer::Silent("timed out".into()));
    let err = sender.send_raw(7, 3, b"raw", &ctx()).await.expect_err("silent");
    assert!(matches!(err, DispatchError::NoResponse { .. }), "{err:?}");
    assert!(c.halted().is_some());
    assert_eq!(c.open_sends().len(), 1, "it may have landed: recovery looks for it");
}

/// Review focus 5: a hold that appears after the gate is honoured at the
/// sender, without counting as an endpoint's refusal and without a record.
#[tokio::test]
async fn a_hold_at_the_sender_is_not_an_endpoint_refusal() {
    let d = tempfile::tempdir().unwrap();
    let (c, t, sender) = rig(d.path(), SendAnswer::Accepted(B256::ZERO));
    std::fs::write(d.path().join("canary.stop"), b"").unwrap();
    let err = sender.send_raw(7, 3, b"raw", &ctx()).await.expect_err("held");
    assert!(matches!(err, DispatchError::Rejected { ref detail, .. } if detail.contains("stop file")), "{err:?}");
    assert_eq!(*t.saw_record.lock().unwrap(), None, "the transport was never called");
    assert_eq!(c.report().totals.refused, 0);
    assert_eq!(c.report().totals.sent, 0);
}

/// Review focus 1: a success naming another transaction is not an acceptance.
#[tokio::test]
async fn a_send_answered_with_another_hash_is_silent() {
    struct OtherHash;
    #[async_trait::async_trait]
    impl RawTransport for OtherHash {
        async fn send_raw(&self, _raw: &[u8]) -> SendAnswer {
            SendAnswer::Accepted(B256::repeat_byte(0x99))
        }
    }
    let d = tempfile::tempdir().unwrap();
    let canary = Arc::new(Canary::open(&d.path().join("l"), &d.path().join("s"), Limits { loss_cap_wei: 1, max_consecutive_reverts: 5, max_consecutive_refusals: 2, max_landed: 100 }).unwrap());
    let sender = CanaryDispatcher::new(Arc::clone(&canary), Arc::new(OtherHash), Arc::new(InFlight::default()));
    let err = sender.send_raw(1, 0, b"raw", &ctx()).await.expect_err("not ours");
    assert!(matches!(err, DispatchError::NoResponse { .. }), "{err:?}");
    let text = std::fs::read_to_string(d.path().join("l")).unwrap();
    assert!(text.lines().any(|l| matches!(serde_json::from_str::<Record>(l), Ok(Record::Outcome(o)) if matches!(o.settled, Settled::Silent { .. }))));
}
```

`SendAnswer` must derive `Clone`. Then, in `crates/apex-chain/src/base/send.rs`, the unit tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot HTTP server answering `body` with `status`.
    async fn serve(status: &'static str, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = s.read(&mut buf).await;
            let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            let _ = s.write_all(reply.as_bytes()).await;
        });
        format!("http://{addr}/v1/rpc/secret-key")
    }

    #[tokio::test]
    async fn an_answer_with_our_hash_is_accepted() {
        let raw = b"\x02signed".to_vec();
        let hash = alloy_primitives::keccak256(&raw);
        let url = serve("200 OK", format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{hash:#x}"}}"#)).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        assert_eq!(s.send_raw(&raw).await, SendAnswer::Accepted(hash));
    }

    #[tokio::test]
    async fn an_error_object_is_a_refusal_with_its_code_and_message() {
        let url = serve("200 OK", r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32605,"message":"flagged"}}"#.into()).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        assert_eq!(s.send_raw(b"x").await, SendAnswer::Refused("-32605: flagged".into()));
    }

    #[tokio::test]
    async fn a_server_error_is_silence_and_names_no_key() {
        let url = serve("502 Bad Gateway", String::new()).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        let SendAnswer::Silent(detail) = s.send_raw(b"x").await else { panic!("not silent") };
        assert!(!detail.contains("secret-key"), "{detail}");
        assert!(!s.label().contains("secret-key"));
    }

    #[tokio::test]
    async fn nothing_listening_is_a_refusal() {
        let s = HttpSubmitter::new("http://127.0.0.1:9/x", Duration::from_secs(2)).unwrap();
        assert!(matches!(s.send_raw(b"x").await, SendAnswer::Refused(_)));
    }
}
```

`apex-chain`'s dev-dependencies already include tokio with `net` and `io-util`.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-chain send 2>&1 | tail -5; cargo test -j 3 -p apex-runtime --test canary_send 2>&1 | tail -5`
Expected: unresolved modules.

- [ ] **Step 3: Implement `crates/apex-chain/src/base/send.rs`:**

```rust
//! The one type in this crate that can spend money (Task 8.5 R28).
//!
//! `rpc`'s transports are read instruments by type: an allowlist refuses
//! every write before a byte leaves. Submission is this separate type, holding
//! one endpoint and one method, `eth_sendRawTransaction`, so the process that
//! reads state for every candidate is not handed a transport that can send.
//!
//! **What an answer means.**
//! - **An error object** is the endpoint's no: nothing was sent.
//! - **A refused connection** reached nothing.
//! - **A timeout or a server error** may or may not have sent it, and is
//!   reported as silence.
//! - **A success naming another hash** is silence too: something may have
//!   been sent, and it was not what we signed.
//!
//! The URL carries the provider key: it is held as a `Secret`, named by
//! [`crate::rpc::endpoint_label`], and stripped from every error (INV-46).

use crate::rpc::endpoint_label;
use alloy_primitives::{keccak256, B256};
use apex_config::Secret;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendAnswer {
    /// The endpoint took it and named our transaction's hash.
    Accepted(B256),
    /// The endpoint answered no. Nothing was sent.
    Refused(String),
    /// No usable answer. It may or may not have been sent.
    Silent(String),
}

#[async_trait::async_trait]
pub trait RawTransport: Send + Sync {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer;
}

pub struct HttpSubmitter {
    url: Secret<String>,
    label: String,
    client: reqwest::Client,
    timeout: Duration,
    next_id: AtomicU64,
}

impl std::fmt::Debug for HttpSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSubmitter").field("endpoint", &self.label).finish_non_exhaustive()
    }
}

impl HttpSubmitter {
    pub fn new(url: &str, timeout: Duration) -> Result<Self, String> {
        let label = endpoint_label(url);
        reqwest::Url::parse(url).map_err(|_| format!("not a URL: {label}"))?;
        let client = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(timeout)
            .build()
            .map_err(|e| e.without_url().to_string())?;
        Ok(Self { url: Secret::new(url.to_string()), label, client, timeout, next_id: AtomicU64::new(1) })
    }

    /// The endpoint, safe to log.
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[async_trait::async_trait]
impl RawTransport for HttpSubmitter {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": "eth_sendRawTransaction",
            "params": [format!("0x{}", alloy_primitives::hex::encode(raw))],
        });
        let response = match self.client.post(self.url.expose().as_str()).timeout(self.timeout).json(&body).send().await {
            Ok(r) => r,
            // Never connected: nothing left.
            Err(e) if e.is_connect() => return SendAnswer::Refused(format!("connect: {}", e.without_url())),
            Err(e) => return SendAnswer::Silent(e.without_url().to_string()),
        };
        let status = response.status();
        if status.is_client_error() {
            return SendAnswer::Refused(format!("http {status}"));
        }
        if !status.is_success() {
            return SendAnswer::Silent(format!("http {status}"));
        }
        let answer: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return SendAnswer::Silent(format!("not JSON-RPC: {}", e.without_url())),
        };
        if let Some(err) = answer.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            return SendAnswer::Refused(format!("{code}: {message}"));
        }
        let ours = keccak256(raw);
        match answer.get("result").and_then(Value::as_str).and_then(|s| s.parse::<B256>().ok()) {
            Some(h) if h == ours => SendAnswer::Accepted(h),
            Some(h) => SendAnswer::Silent(format!("the endpoint named {h:#x}, and ours is {ours:#x}")),
            None => SendAnswer::Silent(format!("no transaction hash in {answer}")),
        }
    }
}
```

Then the `#[cfg(test)] mod tests` from Step 1. Note the first test's accepted hash is the real `keccak256(raw)`, so it passes this check. Add `pub mod send;` to `crates/apex-chain/src/base/mod.rs`.

`crates/apex-runtime/src/canary/send.rs`:

```rust
//! The canary's `Sender` (spec §6): check the hold, write the send record,
//! send, record the answer.

use super::ledger::{Outcome, Sent, Settled};
use super::limits::Canary;
use crate::plane::{SendContext, Sender};
use crate::risk::Hold;
use alloy_primitives::{keccak256, B256};
use apex_capture::dispatch::{AckLadder, DispatchError, DispatchRequest};
use apex_capture::recover::DispatchPermit;
use apex_chain::base::send::{RawTransport, SendAnswer};
use apex_types::ack::LifecycleStage;
use apex_types::ids::SubmissionLaneId;
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The canary's one submission lane.
pub const LANE: SubmissionLaneId = SubmissionLaneId(1);

/// A send in flight: its record, and the receipt once settlement finds it.
#[derive(Clone, Debug)]
pub struct Pending {
    pub sent: Sent,
    pub receipt: Option<serde_json::Value>,
}

/// What the sender knows and settlement reads, by transaction hash.
#[derive(Debug, Default)]
pub struct InFlight(Mutex<BTreeMap<B256, Pending>>);

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<B256, Pending>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn insert(&self, sent: Sent) {
        self.lock().insert(sent.tx, Pending { sent, receipt: None });
    }
    pub fn get(&self, tx: &B256) -> Option<Pending> {
        self.lock().get(tx).cloned()
    }
    pub fn set_receipt(&self, tx: &B256, receipt: serde_json::Value) {
        if let Some(p) = self.lock().get_mut(tx) {
            p.receipt = Some(receipt);
        }
    }
    pub fn remove(&self, tx: &B256) {
        self.lock().remove(tx);
    }
}

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
        .unwrap_or(u64::MAX)
}

pub struct CanaryDispatcher {
    canary: Arc<Canary>,
    transport: Arc<dyn RawTransport>,
    in_flight: Arc<InFlight>,
}

impl CanaryDispatcher {
    pub fn new(canary: Arc<Canary>, transport: Arc<dyn RawTransport>, in_flight: Arc<InFlight>) -> Self {
        Self { canary, transport, in_flight }
    }

    /// The send, without the plane's types: what the fork proof drives.
    pub async fn send_raw(&self, ticket: u64, nonce: u64, raw: &[u8], ctx: &SendContext) -> Result<B256, DispatchError> {
        let rejected = |detail: String| DispatchError::Rejected { lane: LANE, detail };
        // A hold that appeared after the gate: refused here, as a hold and not
        // as an endpoint's refusal, so nothing is recorded.
        if let Some(reason) = self.canary.reason() {
            return Err(rejected(reason));
        }
        let tx = keccak256(raw);
        let sent = Sent {
            ticket,
            nonce,
            tx,
            at_ms: now_ms(),
            chain: ctx.chain,
            strategy: ctx.strategy,
            route_hash: ctx.route_hash,
            venues: ctx.venues.clone(),
            gas_limit: ctx.gas_limit.0,
            max_fee_per_gas: ctx.fees.max_fee_per_gas,
            max_priority_fee_per_gas: ctx.fees.max_priority_fee_per_gas,
            expected_gross_wei: ctx.expected_gross_wei,
            expected_net_wei: ctx.expected_net_wei,
            waived: ctx.waived.clone(),
        };
        // Write-ahead: on disk before a byte leaves.
        self.canary.sent(sent.clone()).map_err(|e| rejected(format!("the canary ledger: {e}")))?;
        self.in_flight.insert(sent);
        let settled = |settled: Settled| Outcome { tx, at_ms: now_ms(), settled };
        match self.transport.send_raw(raw).await {
            SendAnswer::Accepted(_) => Ok(tx),
            SendAnswer::Refused(detail) => {
                self.in_flight.remove(&tx);
                let _ = self.canary.settle(settled(Settled::Refused { detail: detail.clone() }));
                Err(rejected(detail))
            }
            SendAnswer::Silent(detail) => {
                let _ = self.canary.settle(settled(Settled::Silent { detail }));
                Err(DispatchError::NoResponse { lane: LANE })
            }
        }
    }
}

#[async_trait::async_trait]
impl Sender for CanaryDispatcher {
    async fn send(
        &self,
        req: &DispatchRequest,
        _permit: &DispatchPermit<'_>,
        now: UnixNanos,
        context: &SendContext,
    ) -> Result<AckLadder, DispatchError> {
        self.send_raw(req.ticket().0, req.authorization().nonce().get(), &req.signed, context).await?;
        let mut ladder = AckLadder::new(now);
        let _ = ladder.observe(LifecycleStage::TransportAccepted, now);
        Ok(ladder)
    }
}
```

Add `pub mod send;` to `canary/mod.rs`.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -j 3 -p apex-chain send 2>&1 | grep -E "test result|FAILED"; cargo test -j 3 -p apex-runtime --test canary_send 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: ok.

- [ ] **Step 5: Run `scripts/ci/secret_scan.sh` and `scripts/ci/crate_dependency_direction.sh`.** Both must stay green.

- [ ] **Step 6: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - move `self.canary.sent(…)` after `send_raw`;
  - `Some(h) if h == ours` → `Some(h)`;
  - `e.is_connect()` arm → `Silent`;
  - delete the hold check.

- [ ] **Step 7: Commit**

```bash
git add crates/apex-chain/src/base/send.rs crates/apex-chain/src/base/mod.rs crates/apex-runtime/src/canary/send.rs crates/apex-runtime/src/canary/mod.rs crates/apex-runtime/tests/canary_send.rs
git commit -m "feat(canary): a sender that writes before it sends, over the one transport that can (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Settlement and reconciliation

**Files:**
- Create: `crates/apex-runtime/src/canary/chain.rs`
- Modify: `crates/apex-runtime/src/canary/mod.rs` (`pub mod chain;`)
- Modify: `crates/apex-runtime/src/live/adapter.rs` (`Attribution`, `with_attribution`, `reconcile_final_state`)
- Modify: `crates/apex-chain/src/rpc/mod.rs` (`READ_METHODS` gains `"debug_traceTransaction"`)
- Test: `crates/apex-runtime/tests/canary_chain.rs`

**Interfaces:**
- Consumes: `InFlight`, `Pending` (Task 6); `Canary` (Task 5); `AdapterError::Reverted` (Task 2).
- Produces:
  - `pub fn read_receipt(v: &Value, weth: Address, executor: Address, recipients: &[Address]) -> Result<Read, String>`, where `pub struct Read { pub receipt: Receipt, pub paid_wei: u128, pub realised_wei: Option<u128> }`;
  - `pub struct CanaryChain`, with `CanaryChain::new(rpc: Arc<dyn RpcTransport>, canary: Arc<Canary>, in_flight: Arc<InFlight>, s: ChainSettings) -> Self`;
  - `pub struct ChainSettings { pub weth: Address, pub executor: Address, pub trader: Address, pub recipients: Vec<Address>, pub poll: Duration, pub timeout: Duration }`;
  - `impl SettlementFeed for CanaryChain`;
  - `impl live::adapter::Attribution for CanaryChain`;
  - `pub async fn resolve_open(&self, sent: &Sent) -> Resolved`, where `pub enum Resolved { Landed(PnlAttribution), Reverted(RevertClass), NotFound { nonce_consumed: bool } }`. Task 8 uses it.
- `live::adapter`: `#[async_trait] pub trait Attribution: Send + Sync { async fn attribute(&self, tx: B256) -> AdapterResult<PnlAttribution>; }` and `LiveAdapter::with_attribution(self, Arc<dyn Attribution>) -> Self`.

- [ ] **Step 1: Write the failing tests** in `crates/apex-runtime/tests/canary_chain.rs`:

```rust
//! Task 8.5 R28: settlement and reconciliation (`apex_runtime::canary::chain`),
//! over receipts built to Base's format and the executor's events.

use alloy_primitives::{address, Address, B256, U256};
use apex_chain::adapter::AdapterError;
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_runtime::canary::chain::{read_receipt, CanaryChain, ChainSettings, Resolved};
use apex_runtime::canary::ledger::Sent;
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::canary::send::InFlight;
use apex_runtime::live::adapter::Attribution;
use apex_runtime::plane::SettlementFeed;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WETH: Address = address!("4200000000000000000000000000000000000006");
const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
const OWNER: Address = address!("69D54e5fC0b9325D7250f0D0A11690327A3dd8A3");
const TRADER: Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");
const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const REALISED: &str = "0x230bad4a4399a7f96d7dc55ada0f3e590e98017ef0fb0ef2ae55914350e5d832";

fn word(v: u128) -> String {
    format!("{:064x}", v)
}
fn topic_addr(a: Address) -> String {
    format!("0x{:0>64}", alloy_primitives::hex::encode(a))
}

/// A Base receipt for `tx`: the executor pays `fee` and `rest` WETH to the owner
/// and says it realised `realised`.
fn receipt(tx: B256, ok: bool, fee: u128, rest: u128, realised: u128) -> Value {
    let transfer = |amount: u128| json!({
        "address": format!("{WETH:#x}"),
        "topics": [TRANSFER, topic_addr(EXECUTOR), topic_addr(OWNER)],
        "data": format!("0x{}", word(amount)),
    });
    let logs = if ok {
        vec![
            transfer(fee),
            transfer(rest),
            json!({ "address": format!("{EXECUTOR:#x}"), "topics": [REALISED, topic_addr(WETH)], "data": format!("0x{}{}", word(realised), word(fee)) }),
        ]
    } else {
        vec![]
    };
    json!({
        "transactionHash": format!("{tx:#x}"),
        "blockNumber": "0x3200000",
        "status": if ok { "0x1" } else { "0x0" },
        "gasUsed": "0x61a80",            // 400,000
        "effectiveGasPrice": "0x5f5e100", // 0.1 gwei
        "l1Fee": "0x3b9aca00",            // 1 gwei of wei
        "logs": logs,
    })
}

#[test]
fn a_landed_trade_reads_its_gross_from_the_transfers() {
    let tx = B256::repeat_byte(1);
    let r = read_receipt(&receipt(tx, true, 30, 970, 1_000), WETH, EXECUTOR, &[OWNER]).unwrap();
    assert!(r.receipt.success);
    assert_eq!((r.paid_wei, r.realised_wei), (1_000, Some(1_000)));
    assert_eq!(r.receipt.gas_used, 400_000);
    assert_eq!(r.receipt.l1_fee_wei, 1_000_000_000);
}

/// Review focus 2: no `l1Fee`, no reconciliation. A zero would understate the
/// cost of every trade.
#[test]
fn a_receipt_without_an_l1_fee_is_not_reconciled() {
    let mut v = receipt(B256::repeat_byte(1), true, 30, 970, 1_000);
    v.as_object_mut().unwrap().remove("l1Fee");
    assert!(read_receipt(&v, WETH, EXECUTOR, &[OWNER]).is_err());
}

/// Transfers to someone else are not our gross.
#[test]
fn only_transfers_to_the_recipients_count() {
    let r = read_receipt(&receipt(B256::repeat_byte(1), true, 30, 970, 1_000), WETH, EXECUTOR, &[TRADER]).unwrap();
    assert_eq!(r.paid_wei, 0);
}

/// Answers receipts, traces and nonces from a script; counts receipt polls.
struct Node {
    receipt: Mutex<Option<Value>>,
    latest_nonce: u64,
    polls: Mutex<u32>,
}

#[async_trait::async_trait]
impl RpcTransport for Node {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, RpcError> {
        match method {
            "eth_getTransactionReceipt" => {
                *self.polls.lock().unwrap() += 1;
                Ok(self.receipt.lock().unwrap().clone().unwrap_or(Value::Null))
            }
            "eth_getTransactionCount" => Ok(json!(format!("{:#x}", self.latest_nonce))),
            // The executor's MinOutNotMet: Uniswap's "Too little received".
            "debug_traceTransaction" => Ok(json!({ "error": "execution reverted", "output": "0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000001254f6f206c6974746c65207265636569766564000000000000000000000000000000" })),
            other => panic!("unexpected {other}"),
        }
    }
}

fn sent(tx: B256, nonce: u64) -> Sent {
    Sent {
        ticket: 9, nonce, tx, at_ms: 1, chain: ChainId::BASE, strategy: StrategyId(1),
        route_hash: B256::repeat_byte(0x44), venues: vec![VenueId(1)], gas_limit: 500_000,
        max_fee_per_gas: 200_000_000, max_priority_fee_per_gas: 0,
        expected_gross_wei: U256::from(1_000u64), expected_net_wei: 1,
        waived: vec![],
    }
}

fn rig(dir: &std::path::Path, node: Node) -> (Arc<Canary>, Arc<InFlight>, CanaryChain) {
    let canary = Arc::new(Canary::open(&dir.join("l"), &dir.join("s"), Limits { loss_cap_wei: u128::MAX, max_consecutive_reverts: 5, max_consecutive_refusals: 2, max_landed: 100 }).unwrap());
    let in_flight = Arc::new(InFlight::default());
    let chain = CanaryChain::new(
        Arc::new(node),
        Arc::clone(&canary),
        Arc::clone(&in_flight),
        ChainSettings { weth: WETH, executor: EXECUTOR, trader: TRADER, recipients: vec![OWNER], poll: Duration::from_millis(5), timeout: Duration::from_millis(50) },
    );
    (canary, in_flight, chain)
}

#[tokio::test]
async fn a_landed_trade_settles_reconciles_and_is_recorded() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(1);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, true, 30, 970, 1_000_000_000_000))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    let seen = chain.observe(tx).await.unwrap();
    assert!(seen.iter().any(|o| o.stage == apex_types::ack::LifecycleStage::Included));
    let pnl = chain.attribute(tx).await.expect("reconciles");
    // Cost: 400,000 gas at 0.1 gwei, plus the 1 gwei L1 fee.
    let cost = 400_000u128 * 100_000_000 + 1_000_000_000;
    assert_eq!(pnl.gross_profit, 1_000_000_000_000);
    assert_eq!(pnl.net_profit_token, 1_000_000_000_000 - i128::try_from(cost).unwrap());
    assert_eq!(canary.report().totals.landed, 1);
    assert_eq!(canary.open_sends().len(), 0);
}

#[tokio::test]
async fn a_revert_is_classified_recorded_and_returned_as_a_revert() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(2);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, false, 0, 0, 0))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    chain.observe(tx).await.unwrap();
    let err = chain.attribute(tx).await.expect_err("reverted");
    let AdapterError::Reverted { class, gas_used, .. } = err else { panic!("{err:?}") };
    assert_eq!(class, apex_types::sim::RevertClass::MinOutNotMet);
    assert_eq!(gas_used, 400_000);
    let t = canary.report().totals;
    assert_eq!((t.reverted_late, t.reverted_early), (1, 0), "400,000 gas is a late revert");
}

#[tokio::test]
async fn transfers_disagreeing_with_the_event_halt_as_unexplained() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(3);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, true, 30, 970, 999))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    chain.observe(tx).await.unwrap();
    assert!(chain.attribute(tx).await.is_err());
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unexplained { .. })));
}

#[tokio::test]
async fn no_receipt_by_the_timeout_halts_as_unresolved() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(4);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(None), latest_nonce: 0, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    let seen = chain.observe(tx).await.unwrap();
    assert!(seen.is_empty(), "not included");
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unresolved { .. })));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-runtime --test canary_chain 2>&1 | tail -5`
Expected: unresolved `canary::chain` and `live::adapter::Attribution`.

- [ ] **Step 3: Implement.**

`crates/apex-chain/src/rpc/mod.rs`, add to `READ_METHODS` after `"eth_getLogs"`:

```rust
    // R28: a landed revert's reason. A read of a transaction already mined.
    "debug_traceTransaction",
```

`crates/apex-runtime/src/live/adapter.rs`:

```rust
/// What reconciles a transaction that landed (R28): the canary's chain. The
/// shadow has none, and refuses, as `RefusingRpc` always did.
#[async_trait::async_trait]
pub trait Attribution: Send + Sync {
    async fn attribute(&self, tx: B256) -> AdapterResult<PnlAttribution>;
}
```

Add the field `attribution: Option<std::sync::Arc<dyn Attribution>>` to `LiveAdapter` (`None` in `new`), and:

```rust
    /// R28: reconcile through `a`.
    #[must_use]
    pub fn with_attribution(mut self, a: std::sync::Arc<dyn Attribution>) -> Self {
        self.attribution = Some(a);
        self
    }
```

In `reconcile_final_state`:

```rust
    async fn reconcile_final_state(&self, h: B256) -> AdapterResult<PnlAttribution> {
        match &self.attribution {
            Some(a) => a.attribute(h).await,
            None => RefusingRpc.attribute(h).await,
        }
    }
```

`crates/apex-runtime/src/canary/chain.rs`:

```rust
//! Settlement and reconciliation (spec §7): poll the receipt, read what the
//! executor paid, reconcile it, record it.

use super::ledger::{Outcome, Sent, Settled};
use super::limits::Canary;
use super::send::InFlight;
use crate::live::adapter::Attribution;
use crate::plane::{Decline, SettlementFeed};
use alloy_primitives::{Address, B256, U256};
use apex_chain::adapter::{AdapterError, AdapterResult};
use apex_chain::base::observe::{Receipt, TransactionObservation};
use apex_chain::base::reconcile::{reconcile, BalanceDelta, ReconcileInputs};
use apex_chain::rpc::RpcTransport;
use apex_types::cost::GasLimit;
use apex_types::ids::TicketId;
use apex_types::pnl::{OptimizationLayer, PnlAttribution, UsdBounds};
use apex_types::sim::RevertClass;
use apex_types::time::UnixNanos;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const PROFIT_REALISED: &str = "0x230bad4a4399a7f96d7dc55ada0f3e590e98017ef0fb0ef2ae55914350e5d832";
/// A revert under this much gas stopped before the swaps: the deadline, the
/// commitment, the caller (spec §7).
pub const EARLY_REVERT_GAS: u64 = 100_000;

/// A receipt, and what the executor paid and said it realised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub receipt: Receipt,
    /// WETH transferred from the executor to the recipients.
    pub paid_wei: u128,
    /// `ProfitRealised`'s gross, when the executor emitted it.
    pub realised_wei: Option<u128>,
}

fn quantity(v: &Value, field: &str) -> Result<u128, String> {
    v.get(field)
        .and_then(Value::as_str)
        .and_then(|s| u128::from_str_radix(s.strip_prefix("0x")?, 16).ok())
        .ok_or_else(|| format!("the receipt's {field} is missing or not a quantity"))
}

fn topic_address(t: &Value) -> Option<Address> {
    let s = t.as_str()?.strip_prefix("0x")?;
    (s.len() == 64).then(|| s[24..].parse::<Address>().ok()).flatten()
}

fn data_word(log: &Value, i: usize) -> Option<u128> {
    let d = log.get("data")?.as_str()?.strip_prefix("0x")?;
    let w = d.get(i * 64..(i + 1) * 64)?;
    let v = U256::from_str_radix(w, 16).ok()?;
    (v.bit_len() <= 128).then(|| v.to::<u128>())
}

/// Read a Base receipt. **Fails on a missing `l1Fee`**: a zero would understate
/// the cost of every trade (review focus 2).
pub fn read_receipt(v: &Value, weth: Address, executor: Address, recipients: &[Address]) -> Result<Read, String> {
    let tx = v.get("transactionHash").and_then(Value::as_str).and_then(|s| s.parse::<B256>().ok())
        .ok_or("the receipt has no transaction hash")?;
    let receipt = Receipt {
        tx,
        block_number: u64::try_from(quantity(v, "blockNumber")?).map_err(|e| e.to_string())?,
        success: quantity(v, "status")? == 1,
        gas_used: u64::try_from(quantity(v, "gasUsed")?).map_err(|e| e.to_string())?,
        effective_gas_price_wei: quantity(v, "effectiveGasPrice")?,
        l1_fee_wei: quantity(v, "l1Fee")?,
    };
    let logs = v.get("logs").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut paid_wei = 0u128;
    let mut realised_wei = None;
    for log in &logs {
        let address = log.get("address").and_then(Value::as_str).and_then(|s| s.parse::<Address>().ok());
        let topics = log.get("topics").and_then(Value::as_array).cloned().unwrap_or_default();
        let topic0 = topics.first().and_then(Value::as_str).map(str::to_lowercase);
        if address == Some(weth)
            && topic0.as_deref() == Some(TRANSFER)
            && topics.get(1).and_then(topic_address) == Some(executor)
            && topics.get(2).and_then(topic_address).is_some_and(|to| recipients.contains(&to))
        {
            paid_wei = paid_wei.saturating_add(data_word(log, 0).ok_or("a Transfer's amount did not decode")?);
        }
        if address == Some(executor) && topic0.as_deref() == Some(PROFIT_REALISED) {
            realised_wei = Some(data_word(log, 0).ok_or("ProfitRealised's gross did not decode")?);
        }
    }
    Ok(Read { receipt, paid_wei, realised_wei })
}

#[derive(Clone, Debug)]
pub struct ChainSettings {
    pub weth: Address,
    pub executor: Address,
    pub trader: Address,
    pub recipients: Vec<Address>,
    pub poll: Duration,
    pub timeout: Duration,
}

/// What recovery found for a send a crash left open.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Landed(PnlAttribution),
    Reverted(RevertClass),
    NotFound { nonce_consumed: bool },
}

pub struct CanaryChain {
    rpc: Arc<dyn RpcTransport>,
    canary: Arc<Canary>,
    in_flight: Arc<InFlight>,
    s: ChainSettings,
}

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
        .unwrap_or(u64::MAX)
}

fn unix_nanos() -> UnixNanos {
    UnixNanos(u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())).unwrap_or(u64::MAX))
}

impl CanaryChain {
    pub fn new(rpc: Arc<dyn RpcTransport>, canary: Arc<Canary>, in_flight: Arc<InFlight>, s: ChainSettings) -> Self {
        Self { rpc, canary, in_flight, s }
    }

    async fn receipt(&self, tx: B256) -> Option<Value> {
        match self.rpc.call("eth_getTransactionReceipt", json!([tx])).await {
            Ok(v) if !v.is_null() => Some(v),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(%tx, error = %e, "a receipt poll failed; polling on");
                None
            }
        }
    }

    async fn latest_nonce(&self) -> Option<u64> {
        let v = self.rpc.call("eth_getTransactionCount", json!([self.s.trader, "latest"])).await.ok()?;
        u64::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok()
    }

    /// The revert's class from a trace, or `Unknown` where the endpoint will
    /// not trace.
    async fn revert_class(&self, tx: B256) -> RevertClass {
        let Ok(t) = self.rpc.call("debug_traceTransaction", json!([tx, { "tracer": "callTracer" }])).await else {
            return RevertClass::Unknown;
        };
        let data = t.get("output").and_then(Value::as_str)
            .and_then(|s| alloy_primitives::hex::decode(s).ok())
            .unwrap_or_default();
        crate::live::sim::classify(&data, t.get("error").and_then(Value::as_str))
    }

    fn record(&self, tx: B256, settled: Settled) {
        if let Err(e) = self.canary.settle(Outcome { tx, at_ms: now_ms(), settled }) {
            tracing::error!(%tx, error = %e, "the canary ledger refused an outcome");
        }
    }

    /// Reconcile `tx` from the receipt `observe` found (or one fetched now).
    async fn reconcile_tx(&self, sent: &Sent, receipt: Value) -> AdapterResult<PnlAttribution> {
        let tx = sent.tx;
        let read = read_receipt(&receipt, self.s.weth, self.s.executor, &self.s.recipients).map_err(|detail| {
            self.record(tx, Settled::Unexplained { detail: detail.clone() });
            AdapterError::Uninterpretable { detail }
        })?;
        let r = read.receipt;
        let cost = r.total_chain_fee_wei();
        if !r.success {
            let class = self.revert_class(tx).await;
            self.record(tx, Settled::Reverted {
                block: r.block_number,
                gas_used: r.gas_used,
                effective_gas_price_wei: r.effective_gas_price_wei,
                l1_fee_wei: r.l1_fee_wei,
                cost_wei: cost,
                class: format!("{class:?}"),
                early: r.gas_used < EARLY_REVERT_GAS,
            });
            self.in_flight.remove(&tx);
            return Err(AdapterError::Reverted { tx, class, gas_used: r.gas_used, spent_wei: cost });
        }
        if read.realised_wei != Some(read.paid_wei) {
            let detail = format!("paid {} wei, and ProfitRealised says {:?}", read.paid_wei, read.realised_wei);
            self.record(tx, Settled::Unexplained { detail: detail.clone() });
            self.in_flight.remove(&tx);
            return Err(AdapterError::Uninterpretable { detail });
        }
        let inputs = ReconcileInputs {
            ticket: TicketId(sent.ticket),
            chain: sent.chain,
            strategy: sent.strategy,
            venues: sent.venues.clone(),
            route_hash: sent.route_hash,
            optimization_layers: vec![OptimizationLayer::SinglePath],
            profit_token: self.s.weth,
            gas_limit: GasLimit(sent.gas_limit),
            // §2.10: the token decides; no price is invented here.
            usd_bounds: UsdBounds { low: 0.0, high: 0.0 },
            dex_fees_wei: 0,
            flash_fee_wei: 0,
            calldata_bytes: 0,
            compressed_data_estimate: 0,
        };
        let delta = BalanceDelta { token: self.s.weth, delta: i128::try_from(read.paid_wei).unwrap_or(i128::MAX) };
        let pnl = reconcile(&r, &[delta], &inputs).map_err(|e| AdapterError::Uninterpretable { detail: e.to_string() })?;
        self.record(tx, Settled::Landed {
            block: r.block_number,
            gas_used: r.gas_used,
            effective_gas_price_wei: r.effective_gas_price_wei,
            l1_fee_wei: r.l1_fee_wei,
            gross_wei: pnl.gross_profit,
            cost_wei: cost,
            net_wei: pnl.net_profit_token,
        });
        self.in_flight.remove(&tx);
        Ok(pnl)
    }

    /// What a crash left open (spec §9): the receipt if there is one, and
    /// otherwise whether the nonce was consumed.
    pub async fn resolve_open(&self, sent: &Sent) -> Resolved {
        self.in_flight.insert(sent.clone());
        match self.receipt(sent.tx).await {
            Some(v) => match self.reconcile_tx(sent, v).await {
                Ok(pnl) => Resolved::Landed(pnl),
                Err(AdapterError::Reverted { class, .. }) => Resolved::Reverted(class),
                Err(_) => Resolved::NotFound { nonce_consumed: true },
            },
            None => Resolved::NotFound { nonce_consumed: self.latest_nonce().await.is_some_and(|n| n > sent.nonce) },
        }
    }
}

#[async_trait::async_trait]
impl SettlementFeed for CanaryChain {
    async fn observe(&self, tx: B256) -> Result<Vec<TransactionObservation>, Decline> {
        let included = |v: Value| -> Result<Vec<TransactionObservation>, Decline> {
            let read = read_receipt(&v, self.s.weth, self.s.executor, &self.s.recipients)
                .map_err(|detail| Decline::ChainUnavailable { detail })?;
            self.in_flight.set_receipt(&tx, v);
            Ok(vec![TransactionObservation::included(read.receipt, unix_nanos())])
        };
        let deadline = Instant::now() + self.s.timeout;
        loop {
            if let Some(v) = self.receipt(tx).await {
                return included(v);
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(self.s.poll).await;
        }
        // The timeout. If the nonce was consumed, look once more.
        let consumed = match self.in_flight.get(&tx).map(|p| p.sent.nonce) {
            Some(n) => self.latest_nonce().await.is_some_and(|latest| latest > n),
            None => false,
        };
        if consumed {
            if let Some(v) = self.receipt(tx).await {
                return included(v);
            }
        }
        self.record(tx, Settled::Unresolved {
            detail: format!("no receipt within {} ms; nonce consumed: {consumed}", self.s.timeout.as_millis()),
        });
        Ok(Vec::new())
    }
}
```

Then the attribution:

```rust
#[async_trait::async_trait]
impl Attribution for CanaryChain {
    async fn attribute(&self, tx: B256) -> AdapterResult<PnlAttribution> {
        let Some(p) = self.in_flight.get(&tx) else {
            return Err(AdapterError::Uninterpretable { detail: format!("{tx} was not sent by this canary") });
        };
        let receipt = match p.receipt {
            Some(v) => v,
            None => self.receipt(tx).await.ok_or_else(|| AdapterError::Unreachable { detail: format!("no receipt for {tx}") })?,
        };
        self.reconcile_tx(&p.sent, receipt).await
    }
}
```

Add `pub mod chain;` to `canary/mod.rs`. If `Decline::ChainUnavailable` needs `use crate::plane::Decline`, it is already imported.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -j 3 -p apex-runtime --test canary_chain 2>&1 | grep -E "test result|FAILED|panicked"; cargo test -j 3 -p apex-chain 2>&1 | grep -E "test result|FAILED"`
Expected: ok. `the_allowlist_holds_reads_only` must still pass with `debug_traceTransaction`.

- [ ] **Step 5: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - `quantity(v, "l1Fee")?` → `quantity(v, "l1Fee").unwrap_or(0)`;
  - drop the recipients check;
  - `read.realised_wei != Some(read.paid_wei)` → `false`;
  - `r.gas_used < EARLY_REVERT_GAS` → `true`.

- [ ] **Step 6: Commit**

```bash
git add crates/apex-runtime/src/canary/chain.rs crates/apex-runtime/src/canary/mod.rs crates/apex-runtime/src/live/adapter.rs crates/apex-chain/src/rpc/mod.rs crates/apex-runtime/tests/canary_chain.rs
git commit -m "feat(canary): receipts settled and reconciled to the wei, reverts classified (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Recovery at boot

**Files:**
- Create: `crates/apex-runtime/src/canary/recover.rs`
- Modify: `crates/apex-runtime/src/canary/mod.rs` (`pub mod recover;`)
- Test: `crates/apex-runtime/tests/canary_chain.rs` (append)

**Interfaces:**
- Consumes: `CanaryChain::resolve_open` and `Resolved` (Task 7); `Canary::{open_sends, halt}` (Task 5).
- Produces:
  - `pub struct CanaryRecovery`;
  - `CanaryRecovery::build(canary: &Canary, chain: &CanaryChain) -> Self` (async);
  - `impl apex_capture::recover::ChainOutcomeSource for CanaryRecovery`.

- [ ] **Step 1: Write the failing tests**, appended to `tests/canary_chain.rs`:

```rust
use apex_capture::recover::ChainOutcomeSource;
use apex_runtime::canary::recover::CanaryRecovery;
use apex_types::ticket::{TerminalFailure, TicketOutcome, TicketStatus};

fn open_ticket(id: u64) -> apex_types::ticket::OpportunityTicket {
    let mut t = support::ticket_at(TicketStatus::Dispatching);
    t.ticket_id = apex_types::ids::TicketId(id);
    t.status = TicketStatus::Dispatching;
    t
}
```

Add `mod support;` at the top of the file. Its `ticket_at(status)` is the fixture, so `support_ticket()` is `support::ticket_at(TicketStatus::Observed)`. Then:

```rust
#[tokio::test]
async fn a_ticket_with_no_send_record_never_left_the_process() {
    let d = tempfile::tempdir().unwrap();
    let (canary, _f, chain) = rig(d.path(), Node { receipt: Mutex::new(None), latest_nonce: 0, polls: Mutex::new(0) });
    let rec = CanaryRecovery::build(&canary, &chain).await;
    let out = rec.resolve(&open_ticket(5)).unwrap();
    let TicketOutcome::ExplicitFailure { code: TerminalFailure::Abandoned { .. }, cause, .. } = out else { panic!() };
    assert!(cause.contains("no send record"), "{cause}");
    assert_eq!(canary.halted(), None);
}

#[tokio::test]
async fn a_sent_ticket_with_a_receipt_is_reconciled() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(6);
    let (canary, _f, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, true, 30, 970, 1_000_000_000_000))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap(); // ticket 9
    let rec = CanaryRecovery::build(&canary, &chain).await;
    assert!(matches!(rec.resolve(&open_ticket(9)).unwrap(), TicketOutcome::Success { stage: TicketStatus::Reconciled, .. }));
    assert_eq!(canary.report().totals.landed, 1);
}

#[tokio::test]
async fn a_sent_ticket_with_no_receipt_is_abandoned_and_halts() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(7);
    let (canary, _f, chain) = rig(d.path(), Node { receipt: Mutex::new(None), latest_nonce: 0, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    let rec = CanaryRecovery::build(&canary, &chain).await;
    let TicketOutcome::ExplicitFailure { cause, .. } = rec.resolve(&open_ticket(9)).unwrap() else { panic!() };
    assert!(cause.contains("unresolved at boot"), "{cause}");
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unresolved { .. })));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-runtime --test canary_chain 2>&1 | tail -5`
Expected: unresolved `canary::recover`.

- [ ] **Step 3: Implement `crates/apex-runtime/src/canary/recover.rs`:**

```rust
//! Boot recovery for a journal that sent (spec §9, INV-39). Built before the
//! plane's boot from the ledger and the chain, and read synchronously by it.

use super::chain::{CanaryChain, Resolved};
use super::ledger::Halt;
use super::limits::Canary;
use apex_capture::recover::ChainOutcomeSource;
use apex_types::ids::TicketId;
use apex_types::sim::RevertClass;
use apex_types::ticket::{OpportunityTicket, TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

enum Found {
    Landed(Box<apex_types::pnl::PnlAttribution>),
    Reverted(RevertClass),
    Unresolved(String),
}

pub struct CanaryRecovery {
    found: BTreeMap<TicketId, Found>,
}

fn now() -> UnixNanos {
    UnixNanos(u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())).unwrap_or(u64::MAX))
}

impl CanaryRecovery {
    pub async fn build(canary: &Canary, chain: &CanaryChain) -> Self {
        let mut found = BTreeMap::new();
        for sent in canary.open_sends() {
            let f = match chain.resolve_open(&sent).await {
                Resolved::Landed(pnl) => Found::Landed(Box::new(pnl)),
                Resolved::Reverted(class) => Found::Reverted(class),
                Resolved::NotFound { nonce_consumed } => {
                    let _ = canary.halt(Halt::Unresolved { tx: sent.tx });
                    Found::Unresolved(if nonce_consumed {
                        format!("sent as {:#x}; its nonce was consumed by a transaction we cannot find", sent.tx)
                    } else {
                        format!("sent as {:#x}, unresolved at boot", sent.tx)
                    })
                }
            };
            found.insert(TicketId(sent.ticket), f);
        }
        Self { found }
    }
}

impl ChainOutcomeSource for CanaryRecovery {
    fn resolve(&self, t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        let failure = |code, cause: String| TicketOutcome::ExplicitFailure {
            code,
            at: now(),
            state: Box::new(t.state_fingerprint.clone()),
            cause,
        };
        Ok(match self.found.get(&t.ticket_id) {
            None => failure(
                TerminalFailure::Abandoned { at_status: t.status },
                "no send record: it never left the process".to_string(),
            ),
            Some(Found::Landed(pnl)) => TicketOutcome::Success { stage: TicketStatus::Reconciled, realized: pnl.clone() },
            Some(Found::Reverted(class)) => failure(
                TerminalFailure::Reverted { revert_class: *class, data: Vec::new() },
                "reverted on chain before the restart".to_string(),
            ),
            Some(Found::Unresolved(cause)) => failure(TerminalFailure::Abandoned { at_status: t.status }, cause.clone()),
        })
    }
}
```

Add `pub mod recover;` to `canary/mod.rs`.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test -j 3 -p apex-runtime --test canary_chain 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: ok.

- [ ] **Step 5: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - drop the `canary.halt(...)` call;
  - map `None =>` to `Success`.

- [ ] **Step 6: Commit**

```bash
git add crates/apex-runtime/src/canary/recover.rs crates/apex-runtime/src/canary/mod.rs crates/apex-runtime/tests/canary_chain.rs
git commit -m "feat(canary): boot recovery for a journal that sent (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: `apex canary`: configuration, boot wiring, tripwire, report

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs` (`Raw`, `ShadowConfig`, a `CanaryConfig`, `from_yaml_str` split into a shared `parse`, plus `canary_from_yaml_str` and `load_canary`)
- Modify: `crates/apex-runtime/src/shadow/mod.rs` (boot 363–700; the `Shadow` struct; heads loop 905–915; `Report` 1219–1255; `nonce_moved`)
- Modify: `crates/apex-runtime/src/main.rs` (`apex canary`)
- Modify: `scripts/shadow-status.sh`
- Test: `crates/apex-runtime/tests/shadow_parts.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `pub struct CanaryConfig { ledger, stop_file, profit_recipient, loss_cap_wei, max_consecutive_reverts, max_consecutive_refusals, max_landed, receipt_poll_ms, settle_timeout_ms }`;
  - `ShadowConfig::canary: Option<CanaryConfig>`;
  - `ShadowConfig::{canary_from_yaml_str, load_canary}`;
  - `pub fn nonce_beyond_ours(boot: u64, now: u64, sent: u64) -> bool`;
  - `Report::canary: Option<CanaryReport>`.

- [ ] **Step 1: Write the failing tests** in `tests/shadow_parts.rs`:

```rust
fn canary_section() -> &'static str {
    r#"
canary:
  ledger: var/apex/canary.jsonl
  stop_file: var/apex/canary.stop
  profit_recipient: "0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3"
  loss_cap_wei: 1000000000000000
  max_consecutive_reverts: 5
  max_consecutive_refusals: 2
  max_landed: 100
  receipt_poll_ms: 250
  settle_timeout_ms: 10000
"#
}

/// **R28: each command refuses the other's configuration.** The shadow refuses
/// a canary section (it must stay unable to send); the canary refuses a file
/// without one, and a shadow's journal, which recovery would close as never sent.
#[test]
fn each_command_refuses_the_others_configuration() {
    let var = "APEX_SECRET_SHADOW_TEST_CANARY";
    let shadow = yaml(var, "var/apex/shadow.journal");
    let canary = format!("{}{}", yaml(var, "var/apex/canary.journal"), canary_section());
    let e = env(var, SECRET);
    assert!(ShadowConfig::from_yaml_str(&shadow, &e).is_ok());
    assert!(ShadowConfig::from_yaml_str(&canary.replace("canary.journal", "shadow.journal"), &e).is_err(), "a shadow with a canary section");
    assert!(ShadowConfig::canary_from_yaml_str(&shadow.replace("shadow.journal", "canary.journal"), &e).is_err(), "a canary without one");
    assert!(ShadowConfig::canary_from_yaml_str(&format!("{shadow}{}", canary_section()), &e).is_err(), "a canary on a shadow journal");
    let c = ShadowConfig::canary_from_yaml_str(&canary, &e).expect("a canary config");
    let k = c.canary.expect("the section");
    assert_eq!(k.loss_cap_wei, 1_000_000_000_000_000);
    assert_eq!(k.max_consecutive_reverts, 5);
}

/// Review focus 4: the canary's tripwire fires only beyond its own sends.
#[test]
fn the_canary_tripwire_fires_only_beyond_our_sends() {
    use apex_runtime::shadow::nonce_beyond_ours;
    assert!(!nonce_beyond_ours(10, 10, 0));
    assert!(!nonce_beyond_ours(10, 10, 1), "the view lags our send: not a theft");
    assert!(!nonce_beyond_ours(10, 11, 1));
    assert!(nonce_beyond_ours(10, 12, 1));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -j 3 -p apex-runtime --test shadow_parts 2>&1 | tail -5`
Expected: compile errors for `canary_from_yaml_str` and `nonce_beyond_ours`.

- [ ] **Step 3: Implement the configuration** in `config.rs`.

Add:

```rust
/// R28: the live canary. Present only in a canary's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryConfig {
    pub ledger: PathBuf,
    pub stop_file: PathBuf,
    /// The executor must pay here, or the canary refuses to start.
    pub profit_recipient: Address,
    pub loss_cap_wei: u128,
    pub max_consecutive_reverts: u32,
    pub max_consecutive_refusals: u32,
    pub max_landed: u32,
    pub receipt_poll_ms: u64,
    pub settle_timeout_ms: u64,
}

/// Which command is loading.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Shadow,
    Canary,
}
```

Add `#[serde(default)] canary: Option<CanaryConfig>,` to `Raw`, and `pub canary: Option<CanaryConfig>,` to `ShadowConfig`.

Rename the body of `from_yaml_str` to `fn parse(text: &str, env: &Env, command: Command) -> Result<Self, ShadowConfigError>`, and make `from_yaml_str` call `Self::parse(text, env, Command::Shadow)`. Add:

```rust
    /// `ops/canary.base.yaml` (R28).
    pub fn canary_from_yaml_str(text: &str, env: &Env) -> Result<Self, ShadowConfigError> {
        Self::parse(text, env, Command::Canary)
    }

    pub fn load_canary(path: &str, env: &Env) -> Result<Self, ShadowConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ShadowConfigError::Io { path: path.to_string(), detail: e.to_string() })?;
        Self::canary_from_yaml_str(&text, env)
    }
```

In `parse`, replace the journal check with:

```rust
        let named = |p: &PathBuf, what: &str| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(what));
        match (command, &raw.canary) {
            (Command::Shadow, Some(_)) => {
                return Err(invalid("a canary section in a shadow's configuration: `apex shadow` never sends; run `apex canary`"))
            }
            (Command::Shadow, None) if !named(&raw.journal, "shadow") => {
                return Err(invalid("the journal must be named as a shadow's: recovery closes everything in it as never sent"))
            }
            (Command::Canary, None) => return Err(invalid("`apex canary` needs a canary section")),
            (Command::Canary, Some(_)) if !named(&raw.journal, "canary") || named(&raw.journal, "shadow") => {
                return Err(invalid("the journal must be named as a canary's, never a shadow's"))
            }
            (Command::Canary, Some(k))
                if k.loss_cap_wei == 0
                    || k.max_consecutive_reverts == 0
                    || k.max_consecutive_refusals == 0
                    || k.max_landed == 0
                    || k.receipt_poll_ms == 0
                    || k.settle_timeout_ms == 0 =>
            {
                return Err(invalid("canary: every limit and period must be above zero"))
            }
            _ => {}
        }
```

Add `canary: raw.canary,` to the struct literal.

- [ ] **Step 4: Implement the boot wiring** in `shadow/mod.rs`.

Add to the imports: `use crate::canary::chain::{CanaryChain, ChainSettings};`, `use crate::canary::limits::{Canary, CanaryReport, Limits};`, `use crate::canary::recover::CanaryRecovery;`, `use crate::canary::send::{CanaryDispatcher, InFlight};`, `use apex_chain::base::send::HttpSubmitter;`.

Add the tripwire predicate beside `nonce_moved`:

```rust
/// R28: the canary's tripwire. Its own sends move the nonce; a nonce beyond the
/// boot reading plus every send since means the key is in use elsewhere. A view
/// that lags our latest send reads below that, and is not a theft.
pub fn nonce_beyond_ours(boot: u64, now: u64, sent: u64) -> bool {
    now > boot.saturating_add(sent)
}
```

Add the field `canary: Option<Arc<Canary>>,` to `Shadow`.

In `boot`, after `let executor_code = …` and its check, verify where the executor pays:

```rust
    // R28: a canary trades only once the executor pays its profit where the
    // configuration says (the owner's recipient change, ops/host/README.md).
    if let Some(k) = &config.canary {
        let e = config.executor.address;
        let read = reads
            .multicall(&[(e, abi::call0(selector::PROFIT_RECIPIENT)), (e, abi::call0(selector::FEE_RECIPIENT))], head)
            .await
            .map_err(|e| boot_err("the executor's recipients", e))?;
        let at = |i: usize| read.get(i).cloned().flatten().and_then(|d| abi::word_address(&d, 0));
        if at(0) != Some(k.profit_recipient) || at(1) != Some(k.profit_recipient) {
            return Err(ShadowError::Boot(format!(
                "the executor pays profit to {:?} and fees to {:?}, not canary.profit_recipient {}: the owner's recipient change comes first",
                at(0), at(1), k.profit_recipient
            )));
        }
    }
```

Replace the construction of `adapter`, the dispatch lane, the settlement and the risk gate. Build the canary pieces after `let null = …` and `let nothing_sent = …`:

```rust
    // R28: the canary's pieces, when this is `apex canary`.
    let canary = match &config.canary {
        None => None,
        Some(k) => {
            let c = Arc::new(
                Canary::open(&k.ledger, &k.stop_file, Limits {
                    loss_cap_wei: k.loss_cap_wei,
                    max_consecutive_reverts: k.max_consecutive_reverts,
                    max_consecutive_refusals: k.max_consecutive_refusals,
                    max_landed: k.max_landed,
                })
                .map_err(|e| boot_err("the canary ledger", e))?,
            );
            let in_flight = Arc::new(InFlight::default());
            let chain = Arc::new(CanaryChain::new(Arc::clone(&rpc), Arc::clone(&c), Arc::clone(&in_flight), ChainSettings {
                weth: WETH,
                executor: config.executor.address,
                trader: key.address(),
                recipients: vec![k.profit_recipient],
                poll: Duration::from_millis(k.receipt_poll_ms),
                timeout: Duration::from_millis(k.settle_timeout_ms),
            }));
            let submitter = HttpSubmitter::new(config.rpc_http[0].expose(), Duration::from_secs(3))
                .map_err(|e| ShadowError::Boot(format!("the canary's submitter: {e}")))?;
            info!(endpoint = submitter.label(), "the canary sends through this endpoint");
            let dispatcher = Arc::new(CanaryDispatcher::new(Arc::clone(&c), Arc::new(submitter), in_flight));
            Some((c, chain, dispatcher))
        }
    };
```

The adapter: `let adapter = Arc::new({ let a = LiveAdapter::new(…); match &canary { Some((_, chain, _)) => a.with_attribution(Arc::clone(chain) as Arc<dyn crate::live::adapter::Attribution>), None => a } });`.

In `Ports`:

```rust
        dispatch: match &canary {
            Some((_, _, d)) => DispatchLane::Canary(Arc::clone(d) as Arc<dyn crate::plane::Sender>),
            None => DispatchLane::Shadow(Arc::clone(&null)),
        },
```

```rust
        risk: Arc::new({
            let g = LiveRiskGate::new(
                PostureLadder::new(),
                CircuitBreaker::new(U256::MAX, U256::MAX, u32::MAX),
                config.policy.eligibility(),
                Box::new(SystemClock),
            );
            match &canary {
                Some((c, _, _)) => g.with_hold(Arc::clone(c) as Arc<dyn crate::risk::Hold>),
                None => g,
            }
        }),
```

```rust
        settlement: match &canary {
            Some((_, chain, _)) => Arc::clone(chain) as Arc<dyn crate::plane::SettlementFeed>,
            None => nothing_sent.clone(),
        },
```

Replace the boot reconciliation line:

```rust
    let reconciled = match &canary {
        Some((c, chain, _)) => {
            let recovery = CanaryRecovery::build(c, chain).await;
            plane.boot(&recovery, SystemClock.now())
        }
        None => plane.boot(&*nothing_sent, SystemClock.now()),
    }
    .map_err(|e| boot_err("recovery", e))?;
```

In the `Shadow { … }` literal, add `canary: canary.map(|(c, _, _)| c),`. Change the boot log message to `if config.canary.is_some() { "canary booted" } else { "shadow booted" }`; tracing needs a literal, so use two `info!` calls in an `if`.

In the heads loop, after the shadow's nonce check:

```rust
                if let Some(c) = &self.canary {
                    let sent = c.sent_since_boot();
                    if nonce_beyond_ours(boot, v.signer_pending_nonce, sent) {
                        self.nonce_moved.store(true, Ordering::Relaxed);
                        let _ = c.halt(crate::canary::Halt::Tripwire { boot, now: v.signer_pending_nonce, sent });
                    }
                } else if nonce_moved(Some(boot), v.signer_pending_nonce) {
```

Wrap the existing shadow branch in that `else if`, keeping its body.

Add to `Report`: `/// R28: the canary, when this run is one. pub canary: Option<CanaryReport>,`, and fill it with `canary: self.canary.as_ref().map(|c| c.report()),` where the report is built.

`main.rs`: beside the `shadow` arm, add `if std::env::args().nth(1).as_deref() == Some("canary") { return canary().await; }`, plus a `canary()` that copies `shadow()` with three changes:
- `DEFAULT_CANARY_CONFIG: &str = "ops/canary.base.yaml"`;
- `ShadowConfig::load_canary`;
- the messages say "apex canary".

Add `"canary"` to the usage text if `main` prints one.

`scripts/shadow-status.sh`: after the `armed pass` line, follow the file's existing `jq` style:

```bash
canary=$(jq -r 'if .canary then "canary     sent \(.canary.totals.sent) landed \(.canary.totals.landed) reverted \(.canary.totals.reverted_early + .canary.totals.reverted_late) refused \(.canary.totals.refused) net \(.canary.totals.net_wei) wei lost \(.canary.totals.lost_wei) wei open \(.canary.open)\(if .canary.halted then " HALTED: \(.canary.halted)" else "" end)\(if .canary.stop_file then " STOP FILE" else "" end)" else empty end' <<<"$last")
[ -n "$canary" ] && echo "$canary"
```

If the script names its last report differently than `$last`, adapt to the file's variable.

- [ ] **Step 5: Run the tests and the binary's argument check**

Run: `cargo test -j 3 -p apex-runtime --test shadow_parts --test shadow 2>&1 | grep -E "test result|FAILED|panicked"; cargo build -j 3 -p apex-runtime --bin apex 2>&1 | tail -2`
Expected: ok, and a clean build.

- [ ] **Step 6: Mutation check.** Apply each, confirm a failure, restore and `touch`:
  - `(Command::Shadow, Some(_))` arm deleted;
  - `now > boot.saturating_add(sent)` → `now != boot`.

- [ ] **Step 7: Commit**

```bash
git add crates/apex-runtime/src/shadow/config.rs crates/apex-runtime/src/shadow/mod.rs crates/apex-runtime/src/main.rs scripts/shadow-status.sh crates/apex-runtime/tests/shadow_parts.rs
git commit -m "feat(canary): apex canary, wired: the canary lane, its chain, recovery, tripwire and report (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: Ops: the canary's configuration, unit, runbook and owner steps

**Files:**
- Create: `ops/canary.base.yaml`, `ops/host/apex-canary.service`
- Modify: `ops/host/README.md` (a new section, "9. The canary")
- Test: `crates/apex-runtime/tests/shadow_parts.rs` (`the_shipped_canary_config_parses`)

- [ ] **Step 1: Write the failing test**

```rust
/// R28: the shipped canary configuration, as the host will run it.
#[test]
fn the_shipped_canary_config_parses() {
    let path = format!("{}/../../ops/canary.base.yaml", env!("CARGO_MANIFEST_DIR"));
    let c = ShadowConfig::load_canary(&path, &env("APEX_SECRET_BLOCKPI_KEY", "test-only-not-a-key")).unwrap();
    let k = c.canary.expect("a canary section");
    assert_eq!(k.loss_cap_wei, 1_000_000_000_000_000);
    assert_eq!((k.max_consecutive_reverts, k.max_consecutive_refusals, k.max_landed), (5, 2, 100));
    assert_eq!((k.receipt_poll_ms, k.settle_timeout_ms), (250, 10_000));
    assert_eq!(k.profit_recipient, address!("69D54e5fC0b9325D7250f0D0A11690327A3dd8A3"));
    assert_eq!(c.signer.gas_reserve_wei, 100_000_000_000_000);
    assert_eq!(c.policy.max_cost_confidence_bps, 1_000, "a live run keeps the default");
    assert_eq!(c.policy.tip_share_bps, 2_000);
    assert!(c.journal.to_str().unwrap().contains("canary"));
    // Everything else is the shadow's.
    let shadow = ShadowConfig::load(&path.replace("canary", "shadow"), &env("APEX_SECRET_BLOCKPI_KEY", "test-only-not-a-key")).unwrap();
    assert_eq!((c.executor, c.universe, c.lenders.clone()), (shadow.executor, shadow.universe, shadow.lenders));
}
```

- [ ] **Step 2: Run it and watch it fail.** Run: `cargo test -j 3 -p apex-runtime --test shadow_parts the_shipped_canary 2>&1 | tail -5`. Expected: the file does not exist.

- [ ] **Step 3: Create `ops/canary.base.yaml`.**

Copy `ops/shadow.base.yaml`, then change only these parts:
- The header comment becomes `# apex canary on Base (Task 8.5 R28): the reactive lane sends, one ticket at a time, under the limits below. The armed lane stays in shadow. See ops/host/README.md §9.` Keep the placeholder warnings.
- `journal: var/apex/canary.journal`, with the comment `# The canary's own journal: recovery reconciles it against the chain and canary.jsonl.`
- `signer.gas_reserve_wei: 100000000000000`, with the comment `# 0.0001 ETH kept back: the trader starts with 0.0014 (R28).`
- `policy.max_cost_confidence_bps: 1000`, with the comment `# A live run keeps the default (operator, 2026-10-06).`
- A `canary:` section at the end, with these values and comments:

```yaml
# R28: the canary (spec docs/superpowers/specs/2026-10-15-live-canary-design.md).
canary:
  ledger: var/apex/canary.jsonl
  # `touch` it to stop new sends; remove it to resume. Not a halt.
  stop_file: var/apex/canary.stop
  # The executor must pay profit and fees here, or the canary refuses to start.
  profit_recipient: "0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3"
  # 0.001 ETH lost on losing trades; wins do not offset it. A full-cost revert
  # is ~0.0000024 ETH, so ~400 of them.
  loss_cap_wei: 1000000000000000
  max_consecutive_reverts: 5
  max_consecutive_refusals: 2
  max_landed: 100
  receipt_poll_ms: 250
  settle_timeout_ms: 10000
```

- [ ] **Step 4: Create `ops/host/apex-canary.service`.** Copy `apex-shadow.service`, with these differences:
  - **Description:** `APEX canary (Task 8.5 R28): the reactive lane sends real trades under its limits`.
  - **The header comment** says it broadcasts.
  - **In `[Unit]`, add `Conflicts=apex-shadow.service`.** They share the trader key.
  - **The command:** `ExecStart=/opt/apex/bin/apex canary --config ops/canary.base.yaml`.
  - **`EnvironmentFile` stays `/etc/apex/shadow.env`.** It holds the same two secrets.

- [ ] **Step 5: Add §9 to `ops/host/README.md`.** It covers the following.

**a. The owner's two transactions, in order.** Each is prepared with `cast calldata` and dry-run with `cast call --from` as the owner (succeeds) and as the trader (reverts `NotOwner()`), plus `cast estimate`. The commands:

```
$ PR=$(cast calldata "setProfitRecipient(address)" 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3)
$ FR=$(cast calldata "setFeeRecipient(address)" 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3)
$ OUTER=$(cast calldata "multicall(address[],bytes[])" "[0x8940B565D050974b2b589B70De43bCb753b1F93B,0x8940B565D050974b2b589B70De43bCb753b1F93B]" "[$PR,$FR]")
$ cast call --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base   # 0x
$ cast call --from 0xCB436Ba3acb945b3fc8EE6345857262584356595 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base   # NotOwner()
$ cast estimate --from 0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base
$ cast send 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 $OUTER --rpc-url base --interactive     # the owner broadcasts
$ cast call 0x8940B565D050974b2b589B70De43bCb753b1F93B "profitRecipient()(address)" --rpc-url base   # the owner
$ cast send 0xCB436Ba3acb945b3fc8EE6345857262584356595 --value 0.0014ether --rpc-url base --interactive   # fund the trader
$ cast balance 0xCB436Ba3acb945b3fc8EE6345857262584356595 --rpc-url base --ether   # 0.0014
```

**b. The switch from shadow to canary:**
- stop `apex-shadow`;
- archive `var/apex` as `run-10`;
- install `apex-canary.service`;
- `systemctl disable apex-shadow`;
- `systemctl enable --now apex-canary`.

**c. Checks:**
- the boot log line "canary booted";
- the status script's `canary` line;
- the first send's outcome in `var/apex/canary.jsonl`.

**d. The kill switches:**
- **Soft:** `touch /opt/apex/var/apex/canary.stop`.
- **Halt and reset:** `scripts/canary-reset.sh`, with the service stopped.
- **Hard, owner only:** the §6 revocation, or `tripCircuit()` through the router (dry-run the same way). The reader stops sends at the next block either way.

**e. Unwrapping profit WETH.** The owner calls WETH's `withdraw` through `https://mainnet.base.org`, never through BlockPI, which refuses WETH as `to`.

- [ ] **Step 6: Run the test and the owner dry runs.**

Run: `cargo test -j 3 -p apex-runtime --test shadow_parts 2>&1 | grep -E "test result|FAILED"`. Expected: ok.

Then run the three dry-run commands of §9 (a) locally, with `BLOCKPI_KEY` exported from `.env` for foundry's `base` endpoint, never on the command line. Record the results in the README the way §6 records its own: the owner's call returns `0x`, the trader's reverts `NotOwner()`, plus the estimate.

- [ ] **Step 7: Commit**

```bash
git add ops/canary.base.yaml ops/host/apex-canary.service ops/host/README.md crates/apex-runtime/tests/shadow_parts.rs
git commit -m "ops(canary): the canary's configuration, unit, runbook and the owner's steps (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: The fork proof: a real trade, sent, settled and reconciled

**Files:**
- Create: `crates/apex-runtime/tests/canary_fork.rs` (`#[ignore]`, network)

**What it proves:** the sender, settlement and reconciliation over a real profitable trade through the deployed executor, on anvil forked from Base's latest block. It also proves the owner's recipient change: it is applied by impersonation and then read back.

- [ ] **Step 1: Write the test:**

```rust
//! R28's fork proof. Anvil forks Base's latest block from the public endpoint
//! (never the shadow's BlockPI key); nothing touches mainnet.
//!
//! Needs `anvil` and `cast` on PATH. In any tree:
//!   cargo test -j 3 -p apex-runtime --test canary_fork -- --ignored --nocapture

use alloy_primitives::{address, Address, B256, U256};
use apex_chain::base::send::{HttpSubmitter, RawTransport, SendAnswer};
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::RpcTransport;
use apex_config::Secret;
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider};
use apex_exec::encode::PlanEncoder;
use apex_exec::sign::SignedPlan;
use apex_runtime::canary::chain::{CanaryChain, ChainSettings};
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::canary::send::{CanaryDispatcher, InFlight};
use apex_runtime::live::adapter::Attribution;
use apex_runtime::live::book::PoolBook;
use apex_runtime::live::calls::encode_step;
use apex_runtime::live::frontier::{self, BALANCER_VAULT, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::live::reads::ChainReads;
use apex_runtime::plane::{FeeCaps, SendContext, SettlementFeed};
use apex_runtime::sign::{Eip1559Tx, LaneKey};
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const FORK: &str = "https://mainnet.base.org";
const PORT: &str = "8547";
const LOCAL: &str = "http://127.0.0.1:8547";
const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
const ROUTER: Address = address!("9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2");
const OWNER: Address = address!("69D54e5fC0b9325D7250f0D0A11690327A3dd8A3");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI_500: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP_100: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const SWAP_ROUTER02: Address = address!("2626664c2603336E57B271c5C0b26F421741e481");
/// Anvil's first dev account: public, test-only.
const DEV_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const DEV: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

struct Anvil(Child);

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn cast(args: &[&str]) -> String {
    let out = Command::new("cast").args(args).output().expect("cast on PATH");
    assert!(out.status.success(), "cast {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn send_as(from: Address, to: Address, sig: &str, args: &[&str], value: Option<&str>) {
    let (f, t) = (format!("{from:#x}"), format!("{to:#x}"));
    let mut a = vec!["send", "--unlocked", "--from", &f, &t, sig];
    a.extend_from_slice(args);
    if let Some(v) = value {
        a.extend_from_slice(&["--value", v]);
    }
    a.extend_from_slice(&["--rpc-url", LOCAL]);
    cast(&a);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "network: forks Base from the public endpoint; needs anvil and cast"]
async fn a_real_trade_is_sent_settled_and_reconciled_to_the_wei() {
    let _anvil = Anvil(
        Command::new("anvil").args(["--fork-url", FORK, "--port", PORT, "--silent"]).stdout(Stdio::null()).spawn().expect("anvil on PATH"),
    );
    let mut up = false;
    for _ in 0..60 {
        if Command::new("cast").args(["chain-id", "--rpc-url", LOCAL]).output().is_ok_and(|o| o.status.success()) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(up, "anvil did not come up");

    // 1. The owner, impersonated: authorize the dev key and point profit at the owner.
    cast(&["rpc", "anvil_impersonateAccount", &format!("{OWNER:#x}"), "--rpc-url", LOCAL]);
    cast(&["rpc", "anvil_setBalance", &format!("{OWNER:#x}"), "0xde0b6b3a7640000", "--rpc-url", LOCAL]);
    let e = format!("{EXECUTOR:#x}");
    let calls = [
        cast(&["calldata", "setExecutor(address,bool)", &format!("{DEV:#x}"), "true"]),
        cast(&["calldata", "setProfitRecipient(address)", &format!("{OWNER:#x}")]),
        cast(&["calldata", "setFeeRecipient(address)", &format!("{OWNER:#x}")]),
    ];
    send_as(OWNER, ROUTER, "multicall(address[],bytes[])", &[&format!("[{e},{e},{e}]"), &format!("[{}]", calls.join(","))], None);
    assert_eq!(
        cast(&["call", &e, "profitRecipient()(address)", "--rpc-url", LOCAL]).to_lowercase(),
        format!("{OWNER:#x}").to_lowercase()
    );

    // 2. Open a gap: the dev account sells 50 WETH into Uniswap's 0.05% pool.
    let w = format!("{WETH:#x}");
    send_as(DEV, WETH, "deposit()", &[], Some("60ether"));
    send_as(DEV, WETH, "approve(address,uint256)", &[&format!("{SWAP_ROUTER02:#x}"), &U256::MAX.to_string()], None);
    send_as(
        DEV,
        SWAP_ROUTER02,
        "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))",
        &[&format!("({w},{USDC:#x},500,{DEV:#x},50000000000000000000,0,0)")],
        None,
    );

    // 3. The book at the fork's head; the Slipstream-first cycle; its best of a few sizes.
    let rpc: Arc<dyn RpcTransport> =
        Arc::new(FailoverTransport::connect(&[LOCAL.to_string()], 8453, FailoverSettings::default()).await.unwrap());
    let reads = ChainReads::new(Arc::clone(&rpc));
    let head = reads.head().await.unwrap();
    let specs = vec![
        PoolSpec { pool: UNI_500, venue: Venue::UniswapV3, token0: WETH, token1: USDC, fee_ppm: 500, depth_usd: 1e7 },
        PoolSpec { pool: SLIP_100, venue: Venue::Slipstream, token0: WETH, token1: USDC, fee_ppm: 600, depth_usd: 1e7 },
    ];
    let (book, unloaded) = PoolBook::load(&reads, &specs, head).await.unwrap();
    assert!(unloaded.is_empty(), "{unloaded:?}");
    let snap = book.snapshot();
    let (_, cycles) = frontier::build(ChainId::BASE, WETH, &snap);
    let cycle = cycles.values().find(|c| c.legs[0].pool == SLIP_100).expect("the Slipstream-first cycle");
    let live = LiveCycle::new(cycle, &snap).unwrap();
    let one = U256::from(10u64).pow(U256::from(18u64));
    let (size, mid, out) = [1u64, 2, 5, 10, 20]
        .into_iter()
        .filter_map(|n| {
            let s = U256::from(n) * one;
            let [m, o] = live.hop_outputs(u256_to_ethers(s))?.map(u256_to_alloy);
            Some((s, m, o))
        })
        .max_by_key(|(s, _, o)| o.saturating_sub(*s))
        .unwrap();
    assert!(out > size, "the gap pays: {out} for {size}");
    let predicted_gross = out - size;

    // 4. The call, built as the plane builds it, and signed by the dev key.
    let block = cast(&["block", "latest", "--field", "timestamp", "--rpc-url", LOCAL]).parse::<u64>().unwrap();
    let deadline = block + 120;
    let steps = vec![
        encode_step(&cycle.legs[0], &snap[&cycle.legs[0].pool], size, mid, EXECUTOR, deadline).unwrap(),
        encode_step(&cycle.legs[1], &snap[&cycle.legs[1].pool], mid, size + U256::from(1), EXECUTOR, deadline).unwrap(),
    ];
    let loan = Loan { token: WETH, amount: size, provider: LoanProvider::Balancer, provider_addr: BALANCER_VAULT };
    let plan = PlanEncoder::build(loan, steps, 0, U256::from(1), 8453, deadline);
    let declared = plan_commitment(&plan, 8453, EXECUTOR);
    let call = ExecutorCall::from_checked(SignedPlan::check(plan, declared, 8453, EXECUTOR).unwrap());
    let key = LaneKey::from_hex(&Secret::new(DEV_KEY.to_string())).unwrap();
    let nonce = u64::from_str_radix(cast(&["nonce", &format!("{DEV:#x}"), "--rpc-url", LOCAL]).trim_start_matches("0x"), 10).unwrap_or(0);
    let signed = key
        .sign_eip1559(&Eip1559Tx {
            chain_id: 8453,
            nonce,
            max_priority_fee_per_gas: 1_000_000,
            max_fee_per_gas: 2_000_000_000,
            gas_limit: 1_500_000,
            to: call.to(),
            data: call.data().to_vec(),
        })
        .unwrap();

    // 5. Through the canary's own pieces, pointed at the fork.
    let dir = tempfile::tempdir().unwrap();
    let canary = Arc::new(Canary::open(&dir.path().join("canary.jsonl"), &dir.path().join("canary.stop"), Limits {
        loss_cap_wei: 1_000_000_000_000_000,
        max_consecutive_reverts: 5,
        max_consecutive_refusals: 2,
        max_landed: 100,
    }).unwrap());
    let in_flight = Arc::new(InFlight::default());
    let submitter = Arc::new(HttpSubmitter::new(LOCAL, Duration::from_secs(5)).unwrap());
    let sender = CanaryDispatcher::new(Arc::clone(&canary), submitter as Arc<dyn RawTransport>, Arc::clone(&in_flight));
    let ctx = SendContext {
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        route_hash: cycle.commitment.route_hash,
        venues: vec![VenueId(1)],
        gas_limit: GasLimit(1_500_000),
        fees: FeeCaps { max_fee_per_gas: 2_000_000_000, max_priority_fee_per_gas: 1_000_000 },
        expected_gross_wei: predicted_gross,
        expected_net_wei: 0,
        waived: vec!["route_authorization_valid".into()],
    };
    let tx = sender.send_raw(1, nonce, &signed.raw, &ctx).await.expect("anvil took it");
    assert_eq!(tx, signed.hash);
    let chain = CanaryChain::new(Arc::clone(&rpc), Arc::clone(&canary), Arc::clone(&in_flight), ChainSettings {
        weth: WETH,
        executor: EXECUTOR,
        trader: DEV,
        recipients: vec![OWNER],
        poll: Duration::from_millis(200),
        timeout: Duration::from_secs(20),
    });
    let seen = chain.observe(tx).await.unwrap();
    assert!(!seen.is_empty(), "included");
    let pnl = chain.attribute(tx).await.expect("reconciled");
    println!("FORK PROOF: gross {} predicted {predicted_gross}, net {}", pnl.gross_profit, pnl.net_profit_token);
    assert_eq!(U256::from(u128::try_from(pnl.gross_profit).unwrap()), predicted_gross, "realized gross = prediction, to the wei");
    assert_eq!(canary.report().totals.landed, 1);

    // 6. A stale plan (deadline past) reverts early and is counted.
    let stale_plan = PlanEncoder::build(
        Loan { token: WETH, amount: size, provider: LoanProvider::Balancer, provider_addr: BALANCER_VAULT },
        vec![],
        0,
        U256::from(1),
        8453,
        block - 1_000,
    );
    let d2 = plan_commitment(&stale_plan, 8453, EXECUTOR);
    let stale = ExecutorCall::from_checked(SignedPlan::check(stale_plan, d2, 8453, EXECUTOR).unwrap());
    let s2 = key.sign_eip1559(&Eip1559Tx { chain_id: 8453, nonce: nonce + 1, max_priority_fee_per_gas: 1_000_000, max_fee_per_gas: 2_000_000_000, gas_limit: 1_500_000, to: stale.to(), data: stale.data().to_vec() }).unwrap();
    let tx2 = sender.send_raw(2, nonce + 1, &s2.raw, &ctx).await.expect("sent");
    chain.observe(tx2).await.unwrap();
    assert!(chain.attribute(tx2).await.is_err(), "the stale plan reverts");
    assert_eq!(canary.report().totals.reverted_early, 1, "the deadline is checked before the loan");

    // 7. With the stop file present, nothing is sent.
    std::fs::write(dir.path().join("canary.stop"), b"").unwrap();
    assert!(sender.send_raw(3, nonce + 2, &s2.raw, &ctx).await.is_err());
    assert_eq!(canary.report().totals.sent, 2);
}
```

`SendAnswer` is imported for completeness; remove it if unused. If `PlanEncoder::build` refuses an empty step list, give the stale plan the two steps of step 4 with the past deadline. The deadline check still comes first in `_initiateLoanV2`.

- [ ] **Step 2: Compile it, then run it.**

Run: `cargo test -j 3 -p apex-runtime --test canary_fork --no-run 2>&1 | tail -3`. Expected: it compiles.

Then: `cargo test -j 3 -p apex-runtime --test canary_fork -- --ignored --nocapture 2>&1 | tail -15`. Expected: `FORK PROOF: gross … predicted …` with the two equal, and `test result: ok`.

If it fails, debug with `superpowers:systematic-debugging`. Do not loosen the to-the-wei assertion.
- **Anvil's fork is slow or rate-limited by the public endpoint:** retry once, then report it.
- **The gap is too small at the fork's state:** raise the whale swap to 100 WETH.

- [ ] **Step 3: Commit**

```bash
git add crates/apex-runtime/tests/canary_fork.rs
git commit -m "test(canary): the fork proof, a real trade sent, settled and reconciled to the wei (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: Gates, verification, PLAN.md, and the hand-off

**Files:**
- Modify: `PLAN.md` (an R28 entry under Task 8.5, after R27.3's)

- [ ] **Step 1: Run all five gates** with the session's `gates.sh` (scratchpad), or the five commands in Global Constraints. Expected: every script green except the two known-red ones; clippy exit 0; every `test result: ok`; doctests ok; `forge test` ok.

- [ ] **Step 2: Run the mutation sweep again** over every guard named in Tasks 1–9 (`--no-fail-fast`, `touch` after each restore). Record the count killed in PLAN.md.

- [ ] **Step 3: Write the PLAN.md entry,** `- [x] **R28 — the live canary, and a priority fee on the reactive lane.**`, in the style of the R27 entries. It covers:
  - what was built, per task;
  - the INV-17 canary waiver (operator decision, 2026-10-15);
  - **the floor finding reversed:** R6's "raise it" was backwards, and the floor is one wei;
  - the fork proof's numbers, gross against predicted;
  - the mutants killed;
  - the operator's steps before go-live.

- [ ] **Step 4: Commit PLAN.md,** then verify the branch in a clean worktree with its own target directory:

```bash
git add PLAN.md && git commit -m "docs(plan): R28, the live canary (Task 8.5 R28)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git worktree add ../r28-verify task-8.5-r28-canary
cd ../r28-verify && CARGO_TARGET_DIR=/tmp/claude-1000/-home-scotty-arbot-main2-arbot-main-main/5603340d-14a8-4c7a-9dcd-fb16fa5077e6/scratchpad/target-r28 bash <gates.sh>
```

Expected: the same results as Step 1. Then remove the worktree.

- [ ] **Step 5: Hand off with `superpowers:finishing-a-development-branch`.** Merging, pushing and the host deploy are the operator's choices. Report:
  - **What the owner must do first:** the recipient change and the funding (README §9 a).
  - **That deploying ends the shadow's 14-day run** (`run-10`).
  - **That the first send is the BlockPI screen test.**
