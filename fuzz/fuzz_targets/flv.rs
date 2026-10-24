//! FLV / Enhanced RTMP tag inspection, the HEVC CRA→BLA rewrite, and HEVC
//! renumbering over a run of messages.
#![no_main]

use libfuzzer_sys::fuzz_target;
use streamdelay_flv::{hevc, inspect_audio, inspect_video};

fuzz_target!(|data: &[u8]| {
    let _ = inspect_audio(data);
    if let Some(info) = inspect_video(data) {
        if let Some(offset) = info.nal_offset {
            assert!(offset <= data.len());
            if let Some(rewritten) = hevc::cra_to_bla(data, offset) {
                // Only NAL header bytes change, never the length.
                assert_eq!(rewritten.len(), data.len());
            }
        }
    }
    // The same bytes as video messages, each after a 2-byte length, through
    // one guard.
    let mut guard = hevc::PocGuard::new();
    let mut rest = data;
    while let [a, b, tail @ ..] = rest {
        let n = (u16::from_be_bytes([*a, *b]) as usize).min(tail.len());
        let (msg, next) = tail.split_at(n);
        if let Some(out) = guard.video(msg) {
            // Renumbered: the tag's header stays as it was.
            let offset = inspect_video(msg).and_then(|i| i.nal_offset).unwrap();
            assert_eq!(out[..offset], msg[..offset]);
        }
        rest = next;
    }
});
