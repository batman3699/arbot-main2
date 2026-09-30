//! Task 8.5 — **the signer**: key custody (§18.4, INV-46) and EIP-1559 signing.
//!
//! Two halves, tested separately:
//!
//! - **The key** signs byte-for-byte what an independent implementation signs.
//!   ECDSA under RFC 6979 is deterministic, so a correct EIP-1559 signer given the
//!   same key and fields must produce the same bytes as any other correct one.
//!   `fixtures/eip1559_signatures.json` was signed offline by `cast mktx`
//!   (foundry, alloy) with Anvil's published dev key.
//! - **The port** signs the call the plane simulated, at the nonce the
//!   authorization reserved, with the fee caps of the readings — and refuses a
//!   lane it holds no key for, or a call for another chain.

mod support;

use alloy_primitives::{address, hex, keccak256, Address, B256};
use apex_capture::recover::DispatchGate;
use apex_capture::registry::TicketRegistry;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock, NullDispatcher};
use apex_chain::adapter::SignedPayload;
use apex_config::Secret;
use apex_exec::call::ExecutorCall;
use apex_runtime::plane::{
    fee_caps, Decline, DispatchLane, FeeCaps, Handled, LiveReadings, Plane, Ports, Signer,
};
use apex_runtime::sign::{Eip1559Tx, KeyError, LaneKey, LocalSigner};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, SignerLaneId};
use apex_types::ticket::{TerminalFailure, TicketOutcome};
use apex_types::time::UnixNanos;
use ethers_core::types::transaction::eip2718::TypedTransaction;
use std::sync::{Arc, Mutex};
use support::*;

// secret-scan:allow Anvil dev key 0: the published test key every foundry and
// hardhat install ships. It holds nothing and never will; it is here so the
// signature can be compared with an independent signer's.
const ANVIL_DEV_PRIVATE_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const ANVIL0_ADDRESS: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const BOOT: UnixNanos = UnixNanos(1_000_000_000);

fn anvil_key() -> LaneKey {
    LaneKey::from_hex(&Secret::new(ANVIL_DEV_PRIVATE_KEY.to_string())).expect("a valid key")
}

fn fixture() -> serde_json::Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/eip1559_signatures.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read the fixture");
    serde_json::from_str(&text).expect("JSON")
}

fn decode(raw: &[u8]) -> (TypedTransaction, Address) {
    let rlp = ethers_core::utils::rlp::Rlp::new(&raw[1..]);
    assert_eq!(raw[0], 0x02, "not an EIP-1559 envelope");
    let (tx, sig) = TypedTransaction::decode_signed(&ethers_core::utils::rlp::Rlp::new(raw))
        .or_else(|_| {
            // Some decoders take the envelope, some the payload; accept either
            // so the test is about the bytes, not about one decoder's API.
            ethers_core::types::Eip1559TransactionRequest::decode_signed_rlp(&rlp)
                .map(|(t, s)| (TypedTransaction::Eip1559(t), s))
        })
        .expect("the raw transaction decodes");
    let signer = sig.recover(tx.sighash()).expect("the signature recovers");
    (tx, Address::from(signer.0))
}

// ------------------------------------------------------------------ the key

#[test]
fn the_address_is_derived_from_the_key() {
    assert_eq!(anvil_key().address(), ANVIL0_ADDRESS);
}

/// **The differential.** Every case, byte for byte, including an 804-byte
/// `startV2` payload and a non-zero tip.
#[test]
fn signing_matches_an_independent_implementation() {
    let f = fixture();
    let key = anvil_key();
    assert_eq!(f["address"].as_str().unwrap().parse::<Address>().unwrap(), key.address());
    let chain_id = f["chain_id"].as_u64().unwrap();

    for (name, case) in f["cases"].as_object().unwrap() {
        let tx = Eip1559Tx {
            chain_id,
            nonce: case["nonce"].as_u64().unwrap(),
            max_priority_fee_per_gas: u128::from(case["max_priority_fee_per_gas"].as_u64().unwrap()),
            max_fee_per_gas: u128::from(case["max_fee_per_gas"].as_u64().unwrap()),
            gas_limit: case["gas_limit"].as_u64().unwrap(),
            to: case["to"].as_str().unwrap().parse().unwrap(),
            data: hex::decode(case["data"].as_str().unwrap()).unwrap(),
        };
        let signed = key.sign_eip1559(&tx).expect("signs");
        assert_eq!(
            format!("0x{}", hex::encode(&signed.raw)),
            case["raw"].as_str().unwrap(),
            "{name}: the signed bytes differ from the independent signer's"
        );
        assert_eq!(format!("{:?}", signed.hash), case["hash"].as_str().unwrap(), "{name}: hash");
        assert_eq!(signed.hash, keccak256(&signed.raw), "{name}: the hash is of the raw bytes");
    }
}

#[test]
fn a_signature_recovers_to_the_key_address() {
    let key = anvil_key();
    let signed = key.sign_eip1559(&Eip1559Tx {
        chain_id: 8453,
        nonce: 3,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000,
        gas_limit: 900_000,
        to: address!("DbFB219b4F1CE08fA61C5cD3c08C1307760cAec6"),
        data: vec![0x62, 0x90, 0xaa, 0x48],
    })
    .expect("signs");
    let (_, recovered) = decode(&signed.raw);
    assert_eq!(recovered, ANVIL0_ADDRESS);
}

/// INV-46. The key reaches no `Debug`, and a malformed key's error does not echo
/// the input — which would put a key with one typo in it into a log line.
#[test]
fn the_key_reaches_no_debug_output_or_error() {
    let bare = &ANVIL_DEV_PRIVATE_KEY[2..];
    let key = anvil_key();
    assert!(!format!("{key:?}").to_lowercase().contains(bare));

    let signer = LocalSigner::new(ChainId(8453)).with_lane(SignerLaneId(1), anvil_key());
    assert!(!format!("{signer:?}").to_lowercase().contains(bare));

    // One character wrong: still 64 characters, no longer hex.
    let typo = format!("0x{}z", &bare[..63]);
    let err = LaneKey::from_hex(&Secret::new(typo.clone())).unwrap_err();
    assert_eq!(err, KeyError::NotAKey);
    assert!(!format!("{err:?} {err}").contains(&typo[2..60]));
}

/// Surrounding whitespace is not part of a key: the same key pasted with a
/// trailing newline is the same key, with or without its `0x`.
#[test]
fn a_key_is_the_same_key_however_it_is_pasted() {
    for pasted in [format!("  {ANVIL_DEV_PRIVATE_KEY}\n"), ANVIL_DEV_PRIVATE_KEY[2..].to_string(), ANVIL_DEV_PRIVATE_KEY.to_uppercase().replacen("0X", "0x", 1)] {
        let key = LaneKey::from_hex(&Secret::new(pasted)).expect("the same key");
        assert_eq!(key.address(), ANVIL0_ADDRESS);
    }
}

#[test]
fn a_malformed_key_is_refused() {
    for bad in ["", "0x", "0x1234", "not a key", &format!("0x{}", "00".repeat(32))] {
        assert_eq!(
            LaneKey::from_hex(&Secret::new(bad.to_string())).unwrap_err(),
            KeyError::NotAKey,
            "{bad:?} was accepted"
        );
    }
}

// ------------------------------------------------------------------ the port

/// Keeps what the real signer produced, so the payload can be decoded.
struct Capture<S> {
    inner: S,
    signed: Mutex<Vec<(SignedPayload, Vec<u8>, FeeCaps)>>,
}

impl<S: Signer> Signer for Capture<S> {
    fn sign(
        &self,
        auth: &apex_capture::revalidate::SigningAuthorization,
        call: &ExecutorCall,
        gas_limit: GasLimit,
        fees: FeeCaps,
    ) -> Result<SignedPayload, Decline> {
        let out = self.inner.sign(auth, call, gas_limit, fees)?;
        self.signed.lock().unwrap().push((out.clone(), call.data().to_vec(), fees));
        Ok(out)
    }
}

fn pool(lane_address: Address) -> SignerPool {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&EXECUTOR_VERSION.to_be_bytes());
    SignerPool::new(
        ExecutorAuth { chain: BASE, executor: EXECUTOR, executor_version: version },
        vec![LaneConfig {
            id: SignerLaneId(1),
            address: lane_address.into_array(),
            gas_reserve_wei: 1_000_000_000_000_000_000,
        }],
    )
}

fn plane(signer: Arc<dyn Signer>, readings: LiveReadings) -> Plane {
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(
            Box::new(InMemoryJournal::new()),
            Box::new(ManualClock::at(BOOT.0)),
        )),
        pool: Arc::new(pool(ANVIL0_ADDRESS)),
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Shadow(Arc::new(NullDispatcher::new())),
        chain: Arc::new(FakeChain::landing()),
        search: Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        econ: Arc::new(PassThroughEconomics::default()),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        calls: Arc::new(FixtureCalls),
        signer,
        live: Arc::new(FixedReadings(readings)),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");
    plane
}

async fn run_one(plane: &Plane) -> TicketOutcome {
    let handled = plane.on_event(&recorded_stream()[0]).await;
    match &handled[..] {
        [Handled::Closed { outcome, .. }] => (**outcome).clone(),
        other => panic!("expected one closed ticket, got {other:?}"),
    }
}

/// **The port, end to end.** The payload is the simulated call — to the
/// executor, with its calldata, at the reserved nonce, the gas limit and the fee
/// caps — signed by the lane's key, and its hash is the transaction hash.
#[tokio::test]
async fn the_plane_signs_the_simulated_call_with_the_lane_key() {
    let signer = Arc::new(Capture {
        inner: LocalSigner::new(BASE).with_lane(SignerLaneId(1), anvil_key()),
        signed: Mutex::new(Vec::new()),
    });
    let mut r = readings();
    // Not zero: a signer that ignored the reservation and signed nonce 0 would
    // pass against a zero here.
    r.chain_pending_nonce = 7;
    let plane = plane(signer.clone(), r.clone());
    assert!(matches!(run_one(&plane).await, TicketOutcome::ShadowDispatched { .. }));

    let (payload, calldata, fees) = signer.signed.lock().unwrap()[0].clone();
    let (tx, recovered) = decode(&payload.raw);
    assert_eq!(recovered, ANVIL0_ADDRESS, "signed by the lane's key");
    assert_eq!(tx.to_addr().map(|a| Address::from(a.0)), Some(Address::from(EXECUTOR)));
    assert_eq!(tx.data().map(|d| d.to_vec()), Some(calldata));
    assert_eq!(tx.chain_id().map(|c| c.as_u64()), Some(BASE.0));
    assert_eq!(tx.nonce().map(|n| n.as_u64()), Some(r.chain_pending_nonce));
    assert_eq!(tx.gas().map(|g| g.as_u64()), Some(payload.gas_limit.0));
    assert_eq!(fees, fee_caps(&r));
    match &tx {
        TypedTransaction::Eip1559(inner) => {
            assert_eq!(inner.max_fee_per_gas.map(|v| v.as_u128()), Some(fees.max_fee_per_gas));
            assert_eq!(
                inner.max_priority_fee_per_gas.map(|v| v.as_u128()),
                Some(fees.max_priority_fee_per_gas)
            );
        }
        other => panic!("not EIP-1559: {other:?}"),
    }
    assert_eq!(payload.hash, B256::from(keccak256(&payload.raw)));
    assert_eq!(payload.nonce, r.chain_pending_nonce);
    assert_eq!(payload.chain, BASE);
}

fn refused_as_signer_unavailable(outcome: &TicketOutcome) {
    match outcome {
        TicketOutcome::ExplicitFailure { code: TerminalFailure::SignerUnavailable, .. } => {}
        other => panic!("expected SignerUnavailable, got {other:?}"),
    }
}

/// The authorization names a lane; a signer with no key for that lane refuses
/// rather than signing with some other key it holds.
#[tokio::test]
async fn a_lane_without_a_key_is_refused() {
    let signer = LocalSigner::new(BASE).with_lane(SignerLaneId(2), anvil_key());
    let plane = plane(Arc::new(signer), readings());
    refused_as_signer_unavailable(&run_one(&plane).await);
    assert_eq!(plane.misses().by_reason().get("RISK_FAIL"), Some(&1));
}

/// A signer for one chain never signs a call committed for another: the call
/// would revert on the wrong chain at best, and `wrong_chain_submission` is a
/// hard-zero counter.
#[tokio::test]
async fn a_call_for_another_chain_is_refused() {
    let signer = LocalSigner::new(ChainId(1)).with_lane(SignerLaneId(1), anvil_key());
    let plane = plane(Arc::new(signer), readings());
    refused_as_signer_unavailable(&run_one(&plane).await);
}

/// A zero fee cap is a transaction no block will include. Signing it would spend
/// a nonce reservation on nothing, and it can only mean the readings were wrong.
#[tokio::test]
async fn a_zero_fee_cap_is_refused() {
    let signer = LocalSigner::new(BASE).with_lane(SignerLaneId(1), anvil_key());
    let mut r = readings();
    r.observed_fee_wei = 0;
    let plane = plane(Arc::new(signer), r);
    refused_as_signer_unavailable(&run_one(&plane).await);
}
