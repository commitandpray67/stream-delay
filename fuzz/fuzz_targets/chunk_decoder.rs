//! Arbitrary bytes into the chunk decoder, with an arbitrary chunk size.
#![no_main]

use libfuzzer_sys::fuzz_target;
use streamdelay_rtmp::ChunkDecoder;

fuzz_target!(|input: (u16, Vec<u8>, u8)| {
    let (chunk_size, data, split) = input;
    let mut dec = ChunkDecoder::new();
    if dec.set_chunk_size(u32::from(chunk_size).max(1)).is_err() {
        return;
    }
    // Deliver in pieces to exercise every "need more bytes" path.
    let step = usize::from(split).max(1);
    for piece in data.chunks(step) {
        dec.push(piece);
        loop {
            match dec.next_message() {
                Ok(Some(m)) => assert!(m.payload.len() <= 0xFF_FFFF),
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
