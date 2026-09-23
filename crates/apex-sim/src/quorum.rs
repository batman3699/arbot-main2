//! Simulation quorum — the decision, not the transport (§20, Task 4.5).
//!
//! # A quorum is not a vote
//!
//! The name suggests counting. It does not count: **one contradiction vetoes**,
//! however many endpoints confirmed. Three nodes agreeing and one disagreeing
//! is not three-to-one in favour — it is one node saying the trade fails and
//! three saying it does not, and the one may be the only honest observer.
//! Endpoints are not independent witnesses: they run the same client on the
//! same consensus, so agreement is cheap and disagreement is expensive
//! information.
//!
//! This is the same reasoning that gave `apex_state::FeedArbiter` no `Majority`
//! variant. Two feeds echoing one bad upstream are not two witnesses.
//!
//! # Contradiction and unavailability are different answers
//!
//! A verifier that reverts, returns malformed data, or reports a profit below
//! the threshold the primary claimed to clear has **contradicted** the primary.
//! A verifier that times out or fails in transport has said nothing. Conflating
//! them either vetoes on a network blip or ignores a real disagreement, and the
//! two mistakes have opposite costs.
//!
//! # Why this module holds no provider
//!
//! Classifying an answer and concluding from a set of answers are pure. The
//! calls themselves live with the transport. That split is what makes the
//! veto rule testable without a node — and the veto rule is the part that
//! stops a bad trade.

/// What one verifier said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifierVerdict {
    /// Succeeded, and the returned profit meets the primary's threshold.
    Confirmed { profit_wei: u128 },
    /// Succeeded or reverted with a result that contradicts the primary:
    /// revert, malformed return, or profit below the claimed threshold.
    Contradicted { detail: String },
    /// Produced no usable answer — transport failure or timeout. **Not** a
    /// contradiction.
    Unavailable { detail: String },
}

/// How strict the quorum is about needing a confirmation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuorumMode {
    /// No verification.
    Off,
    /// Veto on contradiction; tolerate every verifier being unavailable.
    #[default]
    BestEffort,
    /// Veto on contradiction, and also refuse when nothing confirmed —
    /// dispatching on single-RPC trust is the failure this mode exists to
    /// prevent.
    Strict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuorumOutcome {
    /// Dispatch may proceed. Carries how many endpoints confirmed, which is
    /// information about confidence rather than about permission.
    Proceed { confirmations: usize },
    /// A verifier contradicted the primary simulation.
    Veto { endpoint: String, detail: String },
    /// Strict mode, and nothing confirmed.
    NoConfirmation { unavailable: usize },
}

impl QuorumOutcome {
    pub const fn proceeds(&self) -> bool {
        matches!(self, Self::Proceed { .. })
    }
}

/// Classify a verifier's raw response against the primary's claim.
///
/// `Ok(bytes)` is the returned data; `Err(message)` is the error text. Reverts
/// arrive as call errors, and a revert is a substantive contradiction rather
/// than a transport problem — so the error text is inspected rather than being
/// treated uniformly as unavailability.
pub fn classify(response: Result<&[u8], &str>, min_profit_wei: u128) -> VerifierVerdict {
    match response {
        Ok(raw) => {
            if raw.len() < 32 {
                return VerifierVerdict::Contradicted {
                    detail: format!("returned {} bytes; expected a 32-byte profit", raw.len()),
                };
            }
            // Big-endian, low 16 bytes: a profit above u128 is not a profit,
            // it is a decode error, and saturating there would read as an
            // enormous confirmation.
            let high = &raw[..16];
            if high.iter().any(|b| *b != 0) {
                return VerifierVerdict::Contradicted {
                    detail: "profit does not fit in 128 bits; treating as malformed".into(),
                };
            }
            let mut buf = [0u8; 16];
            buf.copy_from_slice(&raw[16..32]);
            let profit = u128::from_be_bytes(buf);
            if profit >= min_profit_wei {
                VerifierVerdict::Confirmed { profit_wei: profit }
            } else {
                VerifierVerdict::Contradicted {
                    detail: format!("profit {profit} below required {min_profit_wei}"),
                }
            }
        }
        Err(text) => {
            if is_execution_revert(text) {
                VerifierVerdict::Contradicted { detail: text.into() }
            } else {
                VerifierVerdict::Unavailable { detail: text.into() }
            }
        }
    }
}

/// Whether an error text describes the EVM refusing the call rather than the
/// network refusing to carry it.
///
/// Substring matching, and deliberately so. Provider error text is not a
/// stable format, so the exact matching used for on-chain revert *reasons*
/// (`apex_venues::revert`) is not available here — there is nothing exact to
/// match against.
///
/// The bias is chosen rather than inherited. A false `Contradicted` vetoes a
/// good trade: an opportunity lost. A false `Unavailable` lets a real
/// disagreement through: a bad trade dispatched. Those costs are not
/// symmetric, so an ambiguous message is read as a contradiction.
pub fn is_execution_revert(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("revert") || lower.contains("vm execution error")
}

/// Conclude from every verifier's verdict.
///
/// The first contradiction wins, in the order given. Order is the caller's, so
/// a caller that wants a deterministic attribution supplies a deterministic
/// order — and the *decision* is the same either way, because any
/// contradiction vetoes.
pub fn conclude(verdicts: &[(String, VerifierVerdict)], mode: QuorumMode) -> QuorumOutcome {
    if mode == QuorumMode::Off {
        return QuorumOutcome::Proceed { confirmations: 0 };
    }

    let mut confirmations = 0usize;
    let mut unavailable = 0usize;
    for (endpoint, verdict) in verdicts {
        match verdict {
            VerifierVerdict::Confirmed { .. } => confirmations += 1,
            VerifierVerdict::Contradicted { detail } => {
                return QuorumOutcome::Veto {
                    endpoint: endpoint.clone(),
                    detail: detail.clone(),
                }
            }
            VerifierVerdict::Unavailable { .. } => unavailable += 1,
        }
    }

    if mode == QuorumMode::Strict && confirmations == 0 {
        return QuorumOutcome::NoConfirmation { unavailable };
    }
    QuorumOutcome::Proceed { confirmations }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profit_bytes(v: u128) -> Vec<u8> {
        let mut out = vec![0u8; 16];
        out.extend_from_slice(&v.to_be_bytes());
        out
    }

    fn confirmed(n: &str) -> (String, VerifierVerdict) {
        (n.into(), VerifierVerdict::Confirmed { profit_wei: 10 })
    }

    fn contradicted(n: &str) -> (String, VerifierVerdict) {
        (
            n.into(),
            VerifierVerdict::Contradicted {
                detail: "reverted".into(),
            },
        )
    }

    fn unavailable(n: &str) -> (String, VerifierVerdict) {
        (
            n.into(),
            VerifierVerdict::Unavailable {
                detail: "timeout".into(),
            },
        )
    }

    /// The rule the whole module exists for: one contradiction vetoes, however
    /// many confirmed. Endpoints are not independent witnesses.
    #[test]
    fn one_contradiction_vetoes_however_many_confirmed() {
        for confirmations in 0..8 {
            let mut verdicts: Vec<_> = (0..confirmations)
                .map(|i| confirmed(&format!("ok{i}")))
                .collect();
            verdicts.push(contradicted("bad"));
            verdicts.extend((0..confirmations).map(|i| confirmed(&format!("ok{i}b"))));

            assert_eq!(
                conclude(&verdicts, QuorumMode::BestEffort),
                QuorumOutcome::Veto {
                    endpoint: "bad".into(),
                    detail: "reverted".into()
                },
                "{confirmations} confirmations should not outvote one contradiction"
            );
        }
    }

    /// Unavailable is not contradicted. A network blip must not veto.
    #[test]
    fn an_unavailable_verifier_does_not_veto() {
        let verdicts = [confirmed("a"), unavailable("b")];
        assert_eq!(
            conclude(&verdicts, QuorumMode::BestEffort),
            QuorumOutcome::Proceed { confirmations: 1 }
        );
    }

    /// ...but strict mode refuses to dispatch on single-RPC trust.
    #[test]
    fn strict_mode_refuses_when_nothing_confirmed() {
        let verdicts = [unavailable("a"), unavailable("b")];
        assert_eq!(
            conclude(&verdicts, QuorumMode::BestEffort),
            QuorumOutcome::Proceed { confirmations: 0 },
            "best effort tolerates an outage"
        );
        assert_eq!(
            conclude(&verdicts, QuorumMode::Strict),
            QuorumOutcome::NoConfirmation { unavailable: 2 }
        );
    }

    #[test]
    fn a_disabled_quorum_proceeds_without_verifying() {
        assert!(conclude(&[contradicted("bad")], QuorumMode::Off).proceeds());
    }

    /// A profit below the primary's claimed threshold is a contradiction, not
    /// a weaker confirmation.
    #[test]
    fn a_profit_below_the_claim_contradicts() {
        assert_eq!(
            classify(Ok(&profit_bytes(99)), 100),
            VerifierVerdict::Contradicted {
                detail: "profit 99 below required 100".into()
            }
        );
        assert_eq!(
            classify(Ok(&profit_bytes(100)), 100),
            VerifierVerdict::Confirmed { profit_wei: 100 },
            "meeting the threshold exactly confirms"
        );
    }

    /// Malformed return data contradicts rather than being decoded
    /// optimistically.
    #[test]
    fn short_returndata_contradicts() {
        assert!(matches!(
            classify(Ok(&[0u8; 8]), 1),
            VerifierVerdict::Contradicted { .. }
        ));
        assert!(matches!(
            classify(Ok(&[]), 1),
            VerifierVerdict::Contradicted { .. }
        ));
    }

    /// A profit that does not fit in 128 bits is a decode error, not an
    /// enormous confirmation. Saturating would turn a malformed answer into
    /// the most convincing one available.
    #[test]
    fn an_oversized_profit_is_malformed_not_convincing() {
        let mut raw = vec![0xffu8; 16];
        raw.extend_from_slice(&0u128.to_be_bytes());
        assert!(matches!(
            classify(Ok(&raw), 1),
            VerifierVerdict::Contradicted { .. }
        ));
    }

    /// A revert is a contradiction; a transport failure is not. The two
    /// mistakes have opposite costs, so they are classified apart.
    #[test]
    fn a_revert_contradicts_while_a_timeout_does_not() {
        assert!(matches!(
            classify(Err("execution reverted: Too little received"), 1),
            VerifierVerdict::Contradicted { .. }
        ));
        assert!(matches!(
            classify(Err("VM execution error"), 1),
            VerifierVerdict::Contradicted { .. }
        ));
        for transport in [
            "connection refused",
            "timeout after 2s",
            "429 Too Many Requests",
            "dns error",
        ] {
            assert!(
                matches!(classify(Err(transport), 1), VerifierVerdict::Unavailable { .. }),
                "{transport} was read as a contradiction"
            );
        }
    }

    /// Case does not decide whether a revert is a revert.
    #[test]
    fn revert_detection_is_case_insensitive() {
        assert!(is_execution_revert("Execution Reverted"));
        assert!(is_execution_revert("REVERT"));
        assert!(!is_execution_revert("reverse proxy error"));
    }

    /// Substring matching over-reads rather than under-reads, on purpose.
    ///
    /// A transport message that happens to contain "revert" is classified as a
    /// contradiction and vetoes a good trade. The opposite error -- reading a
    /// real contradiction as a timeout -- dispatches a bad one. The costs are
    /// not symmetric, so the ambiguity is resolved toward the veto.
    #[test]
    fn an_ambiguous_message_is_read_as_a_contradiction() {
        assert!(
            matches!(
                classify(Err("failed to reach revert-proxy.example.com"), 1),
                VerifierVerdict::Contradicted { .. }
            ),
            "an ambiguous message must veto rather than be waved through"
        );
    }
}
