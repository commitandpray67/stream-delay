//! FLV / Enhanced RTMP tag inspection and the HEVC CRA→BLA rewrite.
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
});
