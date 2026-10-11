//! R25: Base's public flashblock stream, decoded from frames recorded on
//! 2026-10-12 (block 52,477,080 whole; 52,477,081 indexes 0-3).

use apex_chain::base::flashblock_stream::{decode_frame, FlashblockIndexer, FrameError};
use serde_json::Value;

fn frames() -> Vec<(Vec<u8>, bool, Value)> {
    let f: Value = serde_json::from_str(include_str!("fixtures/flashblocks_base.json")).unwrap();
    f["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                alloy_primitives::hex::decode(x["hex"].as_str().unwrap()).unwrap(),
                x["binary"].as_bool().unwrap(),
                x["expect"].clone(),
            )
        })
        .collect()
}

#[test]
fn every_recorded_frame_decodes_to_what_node_read_from_it() {
    for (bytes, binary, expect) in frames() {
        let f = decode_frame(&bytes, binary).expect("decodes");
        assert_eq!(u64::from(f.index), expect["index"].as_u64().unwrap());
        assert_eq!(f.block, expect["block"].as_u64().unwrap());
        assert_eq!(u64::from(f.transactions), expect["txs"].as_u64().unwrap());
        assert_eq!(f.block_timestamp, expect["base_ts"].as_u64());
    }
}

/// Block 52,477,080's indexes carry 1, 39, 9, 9, 8, 9, 7, 11, 10, 5 and 26
/// transactions: each flashblock's range starts where the last one ended.
#[test]
fn a_whole_block_gets_contiguous_transaction_ranges() {
    let mut ix = FlashblockIndexer::new();
    let seen: Vec<_> = frames()
        .iter()
        .enumerate()
        .filter_map(|(i, (b, bin, _))| ix.observe(&decode_frame(b, *bin).unwrap(), 1_000 + i as u64))
        .collect();
    let firsts: Vec<Option<u32>> = seen.iter().take(11).map(|s| s.first_tx).collect();
    assert_eq!(firsts, [0, 1, 40, 49, 58, 66, 75, 82, 93, 103, 108].map(Some).to_vec());
    assert_eq!(seen[11].first_tx, Some(0), "the next block starts again at 0");
    assert_eq!(seen[11].block_timestamp, Some(1_791_743_509));
    assert_eq!(ix.incomplete_blocks(), 0);
    // A transaction is found by its index: 92 is index 7's last, 93 index 8's first.
    assert_eq!(ix.locate(52_477_080, 92).map(|s| s.index), Some(7));
    assert_eq!(ix.locate(52_477_080, 93).map(|s| s.index), Some(8));
    assert_eq!(ix.locate(52_477_080, 134), None, "past the block's last transaction");
}

/// A block joined mid-way, or with a missing index, has no trustworthy
/// ranges: marked incomplete, ranges unknown, never guessed.
#[test]
fn a_hole_makes_a_block_incomplete() {
    let fr = frames();
    let mut ix = FlashblockIndexer::new();
    let d = |i: usize| decode_frame(&fr[i].0, fr[i].1).unwrap();
    ix.observe(&d(0), 1);
    ix.observe(&d(1), 2);
    let after_hole = ix.observe(&d(3), 3).unwrap(); // index 2 skipped
    assert_eq!(after_hole.first_tx, None);
    assert_eq!(ix.incomplete_blocks(), 1);
    assert_eq!(ix.locate(52_477_080, 50), None);
    let mut late = FlashblockIndexer::new();
    assert_eq!(late.observe(&d(5), 1).unwrap().first_tx, None, "joined at index 5");
    assert!(ix.observe(&d(1), 9).is_none(), "a repeated index is ignored");
}

#[test]
fn a_frame_that_does_not_decode_says_why() {
    let (bytes, _, _) = &frames()[0];
    // A truncated stream fails in the decompressor or, if it yields a prefix, in the JSON.
    assert!(matches!(decode_frame(&bytes[..bytes.len() / 2], true), Err(FrameError::Brotli | FrameError::Json)));
    assert_eq!(decode_frame(b"{\"index\":0}", false), Err(FrameError::Missing("metadata.block_number")));
    assert_eq!(decode_frame(b"not json", false), Err(FrameError::Json));
    // Index 0 names its block twice; the two must agree.
    let frame = |base: &str| {
        format!(r#"{{"index":0,"metadata":{{"block_number":52477080}},"diff":{{"transactions":[]}},"base":{{"block_number":"{base}","timestamp":"0x6ac6a613"}}}}"#)
    };
    assert_eq!(decode_frame(frame("0x320bc98").as_bytes(), false).map(|f| f.block), Ok(52_477_080));
    assert_eq!(decode_frame(frame("0x320bc99").as_bytes(), false), Err(FrameError::Inconsistent));
}
