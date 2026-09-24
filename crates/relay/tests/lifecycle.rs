//! Starting and ending a stream: the moments where a mistake shows on the channel.
//! Each test drives the relay through one way a stream starts or ends and checks
//! what the destination saw.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::*;
use streamdelay_relay::{DelayMode, DestinationKey, EgressStatus};
use streamdelay_rtmp::session::MediaKind;

async fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < end, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Frame ids of the video the destination received, in order.
fn aired(l: &SinkLog) -> Vec<u32> {
    l.media
        .iter()
        .filter(|m| m.kind == MediaKind::Video)
        .filter_map(|m| frame_of(&m.payload))
        .collect()
}

fn assert_consecutive(ids: &[u32]) {
    for w in ids.windows(2) {
        assert_eq!(w[1], w[0] + 1, "frame skipped or repeated: {ids:?}");
    }
}

/// An address nothing listens on.
fn dead_address() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

fn key() -> DestinationKey {
    DestinationKey::Fixed("k".into())
}

// ----- starting ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_starts_one_broadcast_with_headers_and_a_keyframe_first() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    {
        let l = log.lock().unwrap();
        assert_eq!(l.connections, 1);
        assert_eq!(l.published, 1);
        assert_eq!(l.keys, vec!["k".to_string()]);
        assert!(l.metadata >= 1, "no metadata");
        let first_video = l.media.iter().find(|m| m.kind == MediaKind::Video).unwrap();
        assert_eq!(
            &first_video.payload[..2],
            &[0x17, 0x00],
            "decoder configuration must come first"
        );
        let ids = aired(&l);
        assert_eq!(ids[0], 0, "the broadcast must start at the first keyframe");
        assert_consecutive(&ids);
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_delay_holds_everything_back_and_skips_nothing() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(5)).await;
    {
        let l = log.lock().unwrap();
        let ids = aired(&l);
        assert!(!ids.is_empty(), "nothing aired");
        assert_eq!(ids[0], 0, "the start of the stream was skipped");
        assert_consecutive(&ids);
        for m in l.media.iter().filter(|m| m.kind == MediaKind::Video) {
            if let Some(id) = frame_of(&m.payload) {
                let age = m.at.saturating_duration_since(p.captured_at(id));
                assert!(
                    age >= Duration::from_millis(2_950),
                    "frame {id} aired after {age:?}"
                );
            }
        }
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connections_cannot_lock_the_encoder_out() {
    let (sink, _log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    // More connections than there are slots, none of them publishing.
    let mut idle = Vec::new();
    for _ in 0..20 {
        idle.push(
            tokio::net::TcpStream::connect(relay.ingest_addr())
                .await
                .unwrap(),
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut p = Publisher::try_connect(relay.ingest_addr(), "x")
        .await
        .expect("the encoder was locked out");
    p.stream_for(Duration::from_millis(500)).await;
    assert!(relay.state().ingest.connected);
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encoder_reconnecting_replaces_its_silent_old_connection() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    let mut old = Publisher::connect(relay.ingest_addr(), "x").await;
    old.stream_for(Duration::from_secs(1)).await;
    // The encoder's network dies: its connection still looks open, but nothing
    // more arrives. While it was sending a moment ago, it keeps its place.
    let early = Publisher::try_connect(relay.ingest_addr(), "x").await;
    assert!(
        early
            .as_ref()
            .err()
            .is_some_and(|e| e.contains("already streaming")),
        "an active encoder was replaced"
    );
    // Once it has been silent for a while, the encoder's new connection wins.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut new = Publisher::try_connect(relay.ingest_addr(), "x")
        .await
        .expect("the reconnecting encoder was refused");
    new.base = 100_000;
    new.stream_for(Duration::from_secs(3)).await;
    assert!(
        old.closed_by_relay(Duration::from_secs(2)).await,
        "the old connection was left open"
    );
    {
        let l = log.lock().unwrap();
        assert_eq!(l.connections, 1, "the broadcast must continue");
        assert_eq!(l.unpublished, 0);
        let ids = aired(&l);
        let second: Vec<u32> = ids.iter().copied().filter(|i| *i >= 100_000).collect();
        assert!(second.len() > 30, "the new connection barely aired");
        assert_eq!(
            second[0], 100_000,
            "the new connection must start on a keyframe"
        );
    }
    assert!(relay.state().ingest.connected);
    new.stop().await;
    relay.shutdown().await;
}

// ----- ending --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_stream_airs_the_rest_then_ends_the_broadcast_cleanly() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(1)).await;
    relay.set_delay(2_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(4)).await;
    let last = p.frame - 1;
    p.stop().await;
    wait_until("the broadcast ended", Duration::from_secs(10), || {
        relay.state().egress.status == EgressStatus::Idle
    })
    .await;
    wait_until(
        "the destination saw the unpublish",
        Duration::from_secs(2),
        || log.lock().unwrap().unpublished == 1,
    )
    .await;
    let l = log.lock().unwrap();
    assert_eq!(l.connections, 1);
    let ids = aired(&l);
    assert_eq!(ids[0], 0);
    assert_consecutive(&ids);
    assert_eq!(
        *ids.last().unwrap(),
        last,
        "the end of the stream did not air"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_shorter_than_the_delay_still_airs() {
    // A short test stream: it ends, and the grace period runs out, well before
    // the relay would connect to air it (3 s before it is due).
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_millis(500)).await;
    relay.set_delay(7_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_millis(1_500)).await;
    let last = p.frame - 1;
    let first_captured = p.captured_at(0);
    p.stop().await;
    wait_until("the broadcast ended", Duration::from_secs(15), || {
        log.lock().unwrap().unpublished == 1
    })
    .await;
    let l = log.lock().unwrap();
    assert_eq!(l.connections, 1);
    let ids = aired(&l);
    assert_eq!(ids.first(), Some(&0), "the stream did not air: {ids:?}");
    assert_consecutive(&ids);
    assert_eq!(
        *ids.last().unwrap(),
        last,
        "the end of the stream did not air"
    );
    let first = l.media.iter().find(|m| m.kind == MediaKind::Video).unwrap();
    assert!(
        first.at.saturating_duration_since(first_captured) >= Duration::from_millis(6_950),
        "aired before the delay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_in_obs_ends_the_broadcast_without_waiting_for_the_grace_period() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(10)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    let stopped = Instant::now();
    p.stop().await;
    // Nothing is left to air (no delay): viewers must not watch a frozen stream
    // for the 10 s grace period.
    wait_until("the broadcast ended", Duration::from_secs(3), || {
        log.lock().unwrap().unpublished == 1
    })
    .await;
    assert!(
        stopped.elapsed() < Duration::from_secs(2),
        "the broadcast lingered {:?}",
        stopped.elapsed()
    );
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_encoder_keeps_the_broadcast_open_for_the_grace_period() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(3)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    let crashed = Instant::now();
    p.crash();
    // It may come back: the broadcast waits.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        log.lock().unwrap().unpublished,
        0,
        "ended before the grace period"
    );
    assert_eq!(relay.state().egress.status, EgressStatus::Live);
    wait_until("the broadcast ended", Duration::from_secs(5), || {
        log.lock().unwrap().unpublished == 1
    })
    .await;
    assert!(crashed.elapsed() >= Duration::from_secs(3));
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quitting_while_live_ends_the_broadcast_cleanly() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    let quit = Instant::now();
    relay.shutdown().await;
    // Returns once the unpublish is on its way, not after a fixed pause.
    assert!(
        quit.elapsed() < Duration::from_secs(2),
        "shutting down took {:?}",
        quit.elapsed()
    );
    wait_until(
        "the destination saw the unpublish",
        Duration::from_secs(2),
        || log.lock().unwrap().unpublished == 1,
    )
    .await;
    assert_eq!(log.lock().unwrap().connections, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_stream_while_connecting_never_goes_live() {
    // The destination takes its time to accept the stream.
    let opts = SinkOptions {
        publish_delay: Duration::from_secs(3),
        ..Default::default()
    };
    let (sink, log, _kill) = start_sink_with("127.0.0.1:0", opts).await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    let mut asked = false;
    for _ in 0..40 {
        p.stream_for(Duration::from_millis(50)).await;
        asked = !log.lock().unwrap().keys.is_empty();
        if asked {
            break;
        }
    }
    assert!(asked, "the relay never asked to publish");
    relay.end_stream().await.unwrap();
    wait_until(
        "the connection attempt stopped",
        Duration::from_secs(2),
        || relay.state().egress.status == EgressStatus::Idle,
    )
    .await;
    // OBS keeps streaming; nothing may reconnect.
    p.stream_for(Duration::from_secs(4)).await;
    let l = log.lock().unwrap();
    assert_eq!(
        l.abandoned, 1,
        "the relay must give up before the destination answers"
    );
    assert_eq!(l.published, 0, "a broadcast started after End stream");
    assert!(l.media.is_empty());
    assert_eq!(l.connections, 1, "the relay reconnected while ended");
    assert!(relay.state().ended);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destination_down_at_the_end_is_given_up_and_nothing_airs_later() {
    let dead = dead_address();
    let relay = start_relay(dead, key(), Duration::from_millis(500)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    wait_until("the relay is retrying", Duration::from_secs(3), || {
        relay.state().egress.status == EgressStatus::Retrying
    })
    .await;
    p.stop().await;
    wait_until("the relay gave up", Duration::from_secs(5), || {
        relay.state().egress.status == EgressStatus::Idle
    })
    .await;
    let said = relay.state().egress.last_error.unwrap_or_default();
    assert!(said.contains("could not be reached"), "{said}");
    // The destination comes back. What never aired belongs to a finished stream:
    // sending it would start a broadcast (and notify followers).
    let (_, log, _kill) = start_sink_with(&dead.to_string(), SinkOptions::default()).await;
    tokio::time::sleep(Duration::from_secs(11)).await; // longer than any retry backoff
    assert_eq!(
        log.lock().unwrap().connections,
        0,
        "a finished stream went live"
    );

    // The next stream starts fresh.
    let mut next = Publisher::connect(relay.ingest_addr(), "x").await;
    next.base = 100_000;
    next.stream_for(Duration::from_secs(2)).await;
    {
        let ids = aired(&log.lock().unwrap());
        assert!(!ids.is_empty(), "the next stream did not air");
        assert_eq!(
            ids[0], 100_000,
            "the next stream did not start fresh: {ids:?}"
        );
    }
    next.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_stream_whose_destination_is_down_is_given_up_too() {
    let dead = dead_address();
    let relay = start_relay(dead, key(), Duration::from_millis(500)).await;
    relay.set_delay(6_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(1)).await;
    p.stop().await;
    // Nothing is due yet, and the grace period runs out: the relay waits for the
    // stream's turn to air, tries, and gives up once the destination has had the
    // grace period to answer.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(relay.state().egress.status, EgressStatus::Idle);
    // Connecting, or retrying (connecting to a closed port fails at once on some
    // systems and takes a while on others).
    wait_until("the relay tried", Duration::from_secs(8), || {
        relay.state().egress.status != EgressStatus::Idle
    })
    .await;
    wait_until("the relay gave up", Duration::from_secs(8), || {
        relay.state().egress.status == EgressStatus::Idle
    })
    .await;
    let said = relay.state().egress.last_error.unwrap_or_default();
    assert!(said.contains("could not be reached"), "{said}");
    let (_, log, _kill) = start_sink_with(&dead.to_string(), SinkOptions::default()).await;
    tokio::time::sleep(Duration::from_secs(6)).await; // longer than any retry backoff
    assert_eq!(
        log.lock().unwrap().connections,
        0,
        "a finished stream went live"
    );
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_stream_key_is_reported_and_retries_stop_with_the_stream() {
    let opts = SinkOptions {
        reject: true,
        ..Default::default()
    };
    let (sink, log, _kill) = start_sink_with("127.0.0.1:0", opts).await;
    let relay = start_relay(sink, key(), Duration::from_millis(500)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    wait_until("the refusal is reported", Duration::from_secs(3), || {
        let state = relay.state();
        state.egress.status == EgressStatus::Retrying
            && state
                .egress
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("invalid stream key"))
    })
    .await;
    p.stop().await;
    wait_until("the relay gave up", Duration::from_secs(5), || {
        relay.state().egress.status == EgressStatus::Idle
    })
    .await;
    let attempts = log.lock().unwrap().connections;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        log.lock().unwrap().connections,
        attempts,
        "still retrying after the stream ended"
    );
    assert_eq!(log.lock().unwrap().published, 0);
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_stream_after_air_airs_up_to_the_click_and_nothing_after() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    relay.set_delay(2_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(3)).await;
    let last = p.frame - 1;
    let clicked = Instant::now();
    relay.end_stream_after_air().await.unwrap();
    let state = relay.state();
    assert!(state.ending && !state.ended, "{state:?}");
    // OBS keeps streaming; none of that may air.
    p.stream_for(Duration::from_secs(4)).await;
    wait_until("the broadcast ended", Duration::from_secs(3), || {
        log.lock().unwrap().unpublished == 1
    })
    .await;
    let state = relay.state();
    assert!(state.ended && !state.ending, "{state:?}");
    {
        let l = log.lock().unwrap();
        let ids = aired(&l);
        assert_eq!(ids[0], 0);
        assert_consecutive(&ids);
        let end = *ids.last().unwrap();
        assert!(
            end <= last,
            "frame {end} was sent after End stream (at {last})"
        );
        assert!(
            end + 10 >= last,
            "the end was cut short: {end}, clicked at {last}"
        );
        let ended_at = l.media.last().unwrap().at;
        assert!(
            ended_at.saturating_duration_since(clicked) < Duration::from_millis(2_600),
            "the end aired late"
        );
    }
    // Still streaming, and still ended.
    p.stream_for(Duration::from_secs(2)).await;
    let l = log.lock().unwrap();
    assert_eq!(l.connections, 1, "a broadcast started after End stream");
    assert_eq!(l.published, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_stream_after_air_can_be_taken_back_before_the_end_airs() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(4)).await;
    let last = p.frame - 1;
    relay.end_stream_after_air().await.unwrap();
    p.stream_for(Duration::from_secs(1)).await;
    relay.resume().await.unwrap();
    let state = relay.state();
    assert!(!state.ending && !state.ended, "{state:?}");
    p.stream_for(Duration::from_secs(4)).await;
    let l = log.lock().unwrap();
    assert_eq!(l.unpublished, 0, "the broadcast ended anyway");
    assert_eq!(l.connections, 1);
    let ids = aired(&l);
    assert_consecutive(&ids);
    assert!(
        *ids.last().unwrap() > last + 30,
        "the stream stopped airing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarting_in_obs_while_the_end_airs_starts_a_new_broadcast_after_it() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(10)).await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(3)).await;
    let last = p.frame - 1;
    p.stop().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // OBS starts streaming again before the first stream's end has aired.
    let mut next = Publisher::connect(relay.ingest_addr(), "x").await;
    next.base = 100_000;
    next.stream_for(Duration::from_secs(6)).await;
    {
        let l = log.lock().unwrap();
        // The first broadcast gets its whole stream, then ends: continuing it
        // would leave it without data while OBS was stopped.
        assert_eq!(l.unpublished, 1, "the first broadcast did not end");
        assert_eq!(
            l.connections, 2,
            "the new stream did not get a new broadcast"
        );
        assert_eq!(l.published, 2);
        let on = |conn: usize| -> Vec<u32> {
            l.media
                .iter()
                .filter(|m| m.conn == conn && m.kind == MediaKind::Video)
                .filter_map(|m| frame_of(&m.payload))
                .collect()
        };
        let first = on(0);
        assert_eq!(first[0], 0);
        assert_consecutive(&first);
        assert_eq!(
            *first.last().unwrap(),
            last,
            "the first stream's end did not air"
        );
        let second = on(1);
        assert!(second.len() > 30, "the new stream barely aired: {second:?}");
        assert_eq!(
            second[0], 100_000,
            "the new broadcast must start with the new stream"
        );
        assert_consecutive(&second);
        let start = l
            .media
            .iter()
            .find(|m| m.conn == 1 && m.kind == MediaKind::Video)
            .unwrap();
        assert!(
            start
                .at
                .saturating_duration_since(next.captured_at(100_000))
                >= Duration::from_millis(2_950),
            "the new broadcast started without the delay"
        );
    }
    assert!(!relay.state().ended);
    next.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dump_needs_a_delay() {
    let (sink, _log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_millis(500)).await;
    // Live: there is nothing to throw away.
    let refused = relay.dump(DelayMode::Rewind).await;
    assert!(
        matches!(
            refused,
            Err(streamdelay_relay::RelayError::Engine(
                streamdelay_relay::EngineError::NothingToDump
            ))
        ),
        "{refused:?}"
    );
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dump_replays_what_aired_and_never_airs_what_had_not() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    relay.set_delay(2_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(6)).await;
    let dumped = Instant::now();
    let ack = relay.dump(DelayMode::Rewind).await.unwrap();
    assert_eq!(ack.target_ms, 2_000);
    assert!(!relay.state().delay.mask_visible, "a replay needs no slate");
    p.stream_for(Duration::from_secs(5)).await;
    {
        let l = log.lock().unwrap();
        assert_eq!(l.connections, 1, "the broadcast must continue");
        assert_eq!(l.unpublished, 0);
        assert_monotonic(&l);
        let ids = aired(&l);
        for &id in &ids {
            let captured = p.captured_at(id);
            assert!(
                captured + Duration::from_millis(1_800) <= dumped || captured >= dumped,
                "frame {id}, which had not aired at the dump, aired"
            );
        }
        for m in l.media.iter().filter(|m| m.kind == MediaKind::Video) {
            if let Some(id) = frame_of(&m.payload) {
                let age = m.at.saturating_duration_since(p.captured_at(id));
                assert!(
                    age >= Duration::from_millis(1_950),
                    "frame {id} aired after {age:?}"
                );
            }
        }
        let replayed = ids
            .iter()
            .filter(|&&id| ids.iter().filter(|&&x| x == id).count() > 1);
        assert!(replayed.count() > 30, "nothing was replayed");
        let after: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|&id| p.captured_at(id) >= dumped)
            .collect();
        assert!(after.len() > 30, "the stream did not carry on: {after:?}");
        assert_eq!(after[0] % 30, 0, "the stream must carry on from a keyframe");
        assert_consecutive(&after);
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mask_dump_throws_away_what_has_not_aired_under_the_slate() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    relay.set_delay(3_000, DelayMode::Mask).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(5)).await;
    let dumped = Instant::now();
    relay.dump(DelayMode::Mask).await.unwrap();
    assert!(
        relay.state().delay.mask_visible,
        "the slate must go up at once"
    );
    p.stream_for(Duration::from_secs(6)).await;
    let state = relay.state();
    assert!(!state.delay.mask_visible, "the slate stayed up");
    assert!(state.delay.effective_ms >= 2_900, "{:?}", state.delay);
    {
        let l = log.lock().unwrap();
        assert_eq!(l.connections, 1, "the broadcast must continue");
        assert_eq!(l.unpublished, 0);
        for id in aired(&l) {
            let captured = p.captured_at(id);
            assert!(
                captured + Duration::from_millis(2_700) <= dumped || captured >= dumped,
                "frame {id}, which had not aired at the dump, aired"
            );
        }
        let after: Vec<u32> = aired(&l)
            .into_iter()
            .filter(|&id| p.captured_at(id) >= dumped)
            .collect();
        assert!(after.len() > 30, "the stream did not carry on: {after:?}");
        assert_eq!(after[0] % 30, 0, "the stream must carry on from a keyframe");
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_stream_after_air_with_no_delay_ends_at_once_and_cleanly() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(5)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    let last = p.frame - 1;
    relay.end_stream_after_air().await.unwrap();
    p.stream_for(Duration::from_secs(1)).await;
    wait_until("the broadcast ended", Duration::from_secs(1), || {
        log.lock().unwrap().unpublished == 1
    })
    .await;
    assert!(relay.state().ended);
    let l = log.lock().unwrap();
    let ids = aired(&l);
    assert_consecutive(&ids);
    let end = *ids.last().unwrap();
    assert!(
        end <= last && end + 10 >= last,
        "ended at {end}, clicked at {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destination_repeating_the_stream_key_never_gets_it_shown() {
    let opts = SinkOptions {
        reject: true,
        echo_key: true,
        ..Default::default()
    };
    let (sink, _log, _kill) = start_sink_with("127.0.0.1:0", opts).await;
    let secret = "live_424242_DoNotShowThis";
    let relay = start_relay(
        sink,
        DestinationKey::Fixed(secret.into()),
        Duration::from_millis(500),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    wait_until("the refusal is reported", Duration::from_secs(3), || {
        relay
            .state()
            .egress
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("invalid stream key <stream key>"))
    })
    .await;
    assert!(!format!("{:?}", relay.state()).contains("DoNotShowThis"));
    p.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_kept_messages_cannot_hold_more_memory_than_the_buffer_may_use() {
    // Tiny audio messages the buffer keeps, each followed by two large messages
    // the relay drops, which fill the rest of a 1 MiB block. Each tiny message
    // would otherwise keep its whole block allocated.
    let (sink, _log, _kill) = start_sink().await;
    let mut config = relay_config(sink, key(), Duration::from_secs(5));
    config.engine.ram_cap_bytes = 4 * 1024 * 1024;
    let relay = start_relay_with(config).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    for i in 0..100u8 {
        p.send_audio_and_junk(&[0xaf, 0x01, i], 2, 524_280).await;
    }
    // Everything has been taken in once the buffer holds the last message.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let limit = 4 * 1024 * 1024 + 32 * 1024 * 1024;
    let held = relay.ingest_block_bytes();
    assert!(held <= limit, "{held} bytes held, limit {limit}");
    assert!(
        relay.state().delay.buffered_bytes > 0,
        "the audio is still kept"
    );
    p.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_stream_while_a_restarts_previous_end_airs_ends_after_it() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, key(), Duration::from_secs(10)).await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(3)).await;
    let last = p.frame - 1;
    p.stop().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // OBS starts again while the first stream's end airs, and the streamer
    // clicks End stream: the first end airs, and nothing of the new stream does.
    let mut next = Publisher::connect(relay.ingest_addr(), "x").await;
    next.base = 100_000;
    next.stream_for(Duration::from_millis(500)).await;
    relay.end_stream_after_air().await.unwrap();
    assert!(relay.state().ending);
    next.stream_for(Duration::from_secs(5)).await;
    wait_until("the stream ended", Duration::from_secs(3), || {
        relay.state().ended
    })
    .await;
    {
        let l = log.lock().unwrap();
        assert_eq!(l.connections, 1, "a new broadcast started after End stream");
        assert_eq!(l.unpublished, 1);
        let ids = aired(&l);
        assert_consecutive(&ids);
        assert_eq!(
            *ids.last().unwrap(),
            last,
            "the first stream's end did not air"
        );
    }
    next.stop().await;
    relay.shutdown().await;
}
