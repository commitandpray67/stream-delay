//! Arbitrary peer bytes into both RTMP session state machines.
#![no_main]

use libfuzzer_sys::fuzz_target;
use streamdelay_rtmp::session::{
    ClientConfig, ClientSession, ServerConfig, ServerEvent, ServerSession,
};

fuzz_target!(|input: (Vec<u8>, u8)| {
    let (data, split) = input;
    let step = usize::from(split).max(1);

    let mut server = ServerSession::new(ServerConfig::default());
    for piece in data.chunks(step) {
        match server.feed(piece) {
            Ok(events) => {
                if events.iter().any(|e| matches!(e, ServerEvent::PublishRequest { .. })) {
                    server.accept_publish();
                }
            }
            Err(_) => break,
        }
        let _ = server.take_output();
    }

    let mut client = ClientSession::new(ClientConfig::new("app", "rtmp://example/app", "key"));
    for piece in data.chunks(step) {
        if client.feed(piece).is_err() {
            break;
        }
        let _ = client.take_output();
    }
});
