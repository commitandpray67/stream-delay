//! The delay engine under arbitrary (including nonsensical) input: out-of-order
//! timestamps, garbage payloads, commands at any time, reconnects, clock jumps.
//! Invariant: output timestamps never go backwards within a connection.
#![no_main]

use arbitrary::Arbitrary;
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use streamdelay_engine::{Command, DelayMode, Engine, EngineConfig, GoLiveWhen, Kind, OutMsg};

#[derive(Arbitrary, Debug)]
enum Op {
    Advance(u16),
    Video { key: bool, ts_delta: i16, extra: u8 },
    Audio { ts_delta: i16 },
    RawVideo(Vec<u8>),
    Data(Vec<u8>),
    SetDelay { secs: u8, mask: bool },
    GoLive(bool),
    Cancel,
    Disconnect,
    Connect,
    EncoderRestart,
}

fuzz_target!(|ops: Vec<Op>| {
    let mut e = Engine::new(EngineConfig {
        max_delay_ms: 30_000,
        headroom_ms: 2_000,
        ram_cap_bytes: 256 * 1024,
        mask_margin_ms: 500,
    });
    let mut now: u64 = 1_000_000_000;
    let mut ts: i64 = 0;
    let mut out: Vec<OutMsg> = Vec::new();
    let mut last: [Option<u32>; 3] = [None; 3];
    e.ingest_start(now);
    e.output_connected(now);
    let header = Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1]);
    e.ingest(now, Kind::Video, 0, header);

    let check = |out: &mut Vec<OutMsg>, last: &mut [Option<u32>; 3]| {
        for m in out.drain(..) {
            let i = match m.kind {
                Kind::Audio => 0,
                Kind::Video => 1,
                Kind::Data => 2,
            };
            if let Some(prev) = last[i] {
                assert!(m.timestamp >= prev, "{:?} ts went back {prev} -> {}", m.kind, m.timestamp);
            }
            last[i] = Some(m.timestamp);
        }
    };

    for op in ops.into_iter().take(2_000) {
        match op {
            Op::Advance(ms) => now += u64::from(ms) * 1_000,
            Op::Video { key, ts_delta, extra } => {
                ts = (ts + i64::from(ts_delta)).max(0);
                let b0 = if key { 0x17 } else { 0x27 };
                let p = Bytes::from(vec![b0, 0x01, 0, 0, extra, 0, 0, 0, 1, extra]);
                e.ingest(now, Kind::Video, ts as u64, p);
            }
            Op::Audio { ts_delta } => {
                let t = (ts + i64::from(ts_delta)).max(0);
                e.ingest(now, Kind::Audio, t as u64, Bytes::from_static(&[0xaf, 0x01, 0x21]));
            }
            Op::RawVideo(p) => e.ingest(now, Kind::Video, ts.max(0) as u64, Bytes::from(p)),
            Op::Data(p) => e.ingest(now, Kind::Data, ts.max(0) as u64, Bytes::from(p)),
            Op::SetDelay { secs, mask } => {
                let mode = if mask { DelayMode::Mask } else { DelayMode::Rewind };
                let _ = e.command(now, Command::SetDelay { ms: u64::from(secs) * 1000, mode });
            }
            Op::GoLive(after) => {
                let when = if after { GoLiveWhen::AfterAir } else { GoLiveWhen::Now };
                let _ = e.command(now, Command::GoLive(when));
            }
            Op::Cancel => {
                let _ = e.command(now, Command::Cancel);
            }
            Op::Disconnect => e.output_disconnected(now, None),
            Op::Connect => {
                e.output_connected(now);
                // A new connection restarts timestamps at 0.
                last = [None; 3];
            }
            Op::EncoderRestart => {
                e.ingest_end(now);
                e.ingest_start(now);
                ts = 0;
            }
        }
        let _ = e.poll(now, &mut out);
        check(&mut out, &mut last);
        let _ = e.snapshot(now);
    }
});
