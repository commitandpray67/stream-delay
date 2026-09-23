//! End-to-end tests over real sockets: a synthetic publisher streams through the relay
//! to an in-process RTMP sink.

mod common;

use std::time::{Duration, Instant};

use common::*;
use streamdelay_relay::{
    DelayMode, Destination, DestinationKey, EgressStatus, GoLiveWhen, RelayConfig,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passthrough_is_byte_faithful_and_immediate() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("twitch-key".into()),
        Duration::from_secs(1),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "anything").await;
    p.stream_for(Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let l = log.lock().unwrap();
        assert_eq!(l.keys, vec!["twitch-key".to_string()]);
        assert_eq!(l.metadata, 1);
        // Decoder configuration first, then frames in order and unmodified.
        assert_eq!(&l.media[0].payload[..2], &[0x17, 0x00]);
        let frames = video_frames(&l);
        assert!(frames.len() > 60, "only {} frames", frames.len());
        assert!(
            frames.windows(2).all(|w| w[1].1 == w[0].1 + 1),
            "frames skipped or repeated"
        );
        for m in l.media.iter().filter(|m| frame_of(&m.payload).is_some()) {
            assert_eq!(m.payload, video_payload(frame_of(&m.payload).unwrap()));
        }
        assert_monotonic(&l);
    }
    let state = relay.state();
    assert_eq!(state.egress.status, EgressStatus::Live);
    assert!(state.ingest.connected);
    p.stop().await;
    // The broadcast ends after the grace period.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(log.lock().unwrap().unpublished, 1);
    assert_eq!(relay.state().egress.status, EgressStatus::Idle);
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delay_rewind_and_go_live() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(4)).await;

    let ack = relay.set_delay(2_000, DelayMode::Rewind).await.unwrap();
    assert!((2_000..=3_100).contains(&ack.effective_ms), "{ack:?}");
    let t_rewind = Instant::now();
    p.stream_for(Duration::from_secs(3)).await;
    {
        let l = log.lock().unwrap();
        let frames = video_frames(&l);
        // Viewers see some frames again, starting from a keyframe. Find the rewind
        // as the backward jump in what the sink received: frames sent just before
        // the splice can still arrive after the command returns.
        let jump = frames
            .windows(2)
            .position(|w| w[1].1 < w[0].1)
            .map(|i| i + 1)
            .expect("no rewind happened");
        let (from, to) = (frames[jump - 1].1, frames[jump].1);
        assert_eq!(to % 30, 0, "rewind did not land on a keyframe");
        assert!(from - to >= 30, "rewound only {} frames", from - to);
        assert!(
            frames[jump].0 + Duration::from_millis(500) >= t_rewind,
            "rewind happened before the command"
        );
        assert_monotonic(&l);
    }
    assert_eq!(relay.state().delay.phase, streamdelay_relay::Phase::Delayed);

    relay.go_live(GoLiveWhen::Now).await.unwrap();
    p.stream_for(Duration::from_millis(1500)).await;
    let state = relay.state();
    assert_eq!(
        state.delay.phase,
        streamdelay_relay::Phase::Live,
        "{:?}",
        state.delay
    );
    assert!(state.delay.effective_ms < 200);
    assert_monotonic(&log.lock().unwrap());
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_reconnects_after_a_drop() {
    let (sink, log, kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(5)).await;
    kill.send(true).unwrap();
    p.stream_for(Duration::from_secs(4)).await;
    {
        let l = log.lock().unwrap();
        assert!(l.connections >= 2, "relay did not reconnect");
        assert_eq!(l.keys.len(), l.connections);
        // Media continued after the reconnect.
        let frames = video_frames(&l);
        let last = frames.last().unwrap().1;
        assert!(last > 100, "stream stalled at frame {last}");
    }
    assert!(relay.state().egress.reconnects >= 1);
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passthrough_key_and_second_publisher_rejected() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, DestinationKey::Passthrough, Duration::from_secs(1)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "live_123?bandwidthtest=true").await;
    p.stream_for(Duration::from_secs(1)).await;
    // A second encoder is refused while the first is live.
    let second = tokio::spawn(Publisher::connect(relay.ingest_addr(), "other"));
    let result = second.await;
    assert!(
        result.is_err_and(|e| e.is_panic()),
        "second publisher was accepted"
    );
    p.stream_for(Duration::from_millis(500)).await;
    assert_eq!(
        log.lock().unwrap().keys,
        vec!["live_123?bandwidthtest=true".to_string()]
    );
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_key_is_enforced() {
    let (sink, _log, _kill) = start_sink().await;
    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        ingest_key: Some("let-me-in".into()),
        destination: Some(Destination {
            url: format!("rtmp://{sink}/app"),
            key: DestinationKey::Fixed("k".into()),
        }),
        ..Default::default()
    })
    .await
    .unwrap();
    let addr = relay.ingest_addr();
    let wrong = tokio::spawn(async move { Publisher::connect(addr, "guess").await });
    assert!(
        wrong.await.is_err_and(|e| e.is_panic()),
        "wrong key was accepted"
    );
    assert!(
        relay
            .state()
            .ingest
            .last_error
            .unwrap()
            .contains("wrong stream key")
    );
    let p = Publisher::connect(addr, "let-me-in").await;
    assert!(relay.state().ingest.connected);
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_releases_the_ingest_port() {
    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
    .unwrap();
    let addr = relay.ingest_addr();
    relay.shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The port can be bound again, so a restarted app gets the same address.
    let again = streamdelay_relay::start(RelayConfig {
        ingest_bind: addr,
        ..Default::default()
    })
    .await;
    assert!(again.is_ok(), "ingest port still held after shutdown");
}

#[tokio::test]
async fn network_ingest_requires_a_key() {
    let exposed = streamdelay_relay::start(RelayConfig {
        ingest_bind: "0.0.0.0:0".parse().unwrap(),
        ..Default::default()
    })
    .await;
    assert!(matches!(
        exposed,
        Err(streamdelay_relay::RelayError::IngestKeyRequired(_))
    ));
    let empty_key = streamdelay_relay::start(RelayConfig {
        ingest_bind: "0.0.0.0:0".parse().unwrap(),
        ingest_key: Some(String::new()),
        ..Default::default()
    })
    .await;
    assert!(empty_key.is_err(), "an empty key is no key");
    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "0.0.0.0:0".parse().unwrap(),
        ingest_key: Some("k".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_that_never_publish_are_closed() {
    use tokio::io::AsyncReadExt;
    let relay = streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        publish_timeout: Duration::from_millis(300),
        ..Default::default()
    })
    .await
    .unwrap();
    // Connects and sends nothing, holding a connection slot.
    let mut idle = tokio::net::TcpStream::connect(relay.ingest_addr())
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(5), idle.read(&mut buf))
        .await
        .expect("an idle connection was kept open");
    assert!(matches!(read, Ok(0) | Err(_)));
    for _ in 0..50 {
        if relay.state().ingest.last_error.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        relay
            .state()
            .ingest
            .last_error
            .unwrap()
            .contains("did not start publishing")
    );
    // A real encoder publishes in time and keeps streaming past the deadline.
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_millis(900)).await;
    assert!(relay.state().ingest.connected);
    p.stop().await;
    relay.shutdown().await;
}
