//! AMF0 decoding, plus a round trip: whatever decodes must re-encode stably.
#![no_main]

use libfuzzer_sys::fuzz_target;
use streamdelay_rtmp::amf0;

fuzz_target!(|data: &[u8]| {
    let _ = amf0::decode_one(data);
    if let Ok(values) = amf0::decode_all(data) {
        let encoded = amf0::encode_all(&values);
        let again = amf0::decode_all(&encoded).expect("our own encoding must decode");
        // Compare encodings rather than values: NaN != NaN.
        assert_eq!(amf0::encode_all(&again), encoded);
    }
});
