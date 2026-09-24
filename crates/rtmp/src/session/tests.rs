use bytes::Bytes;

use super::*;
use crate::amf0::Amf0Value;

/// Shuttles bytes between a client and a server until both are quiet.
fn pump(c: &mut ClientSession, s: &mut ServerSession) -> (Vec<ClientEvent>, Vec<ServerEvent>) {
    let mut ce = Vec::new();
    let mut se = Vec::new();
    loop {
        let to_s = c.take_output();
        let to_c = s.take_output();
        if to_s.is_empty() && to_c.is_empty() {
            return (ce, se);
        }
        se.extend(s.feed(&to_s).unwrap());
        ce.extend(c.feed(&to_c).unwrap());
    }
}

fn connected_pair() -> (ClientSession, ServerSession) {
    let mut cfg = ClientConfig::new("live", "rtmp://127.0.0.1/live", "secret?bandwidthtest=true");
    cfg.extra_connect_props = vec![(
        "fourCcList".into(),
        Amf0Value::StrictArray(vec![Amf0Value::string("hvc1")]),
    )];
    let mut c = ClientSession::new(cfg);
    let mut s = ServerSession::new(ServerConfig::default());
    let (_, se) = pump(&mut c, &mut s);
    match &se[..] {
        [
            ServerEvent::Connect { app, props, .. },
            ServerEvent::PublishRequest { stream_key, .. },
        ] => {
            assert_eq!(app, "live");
            assert_eq!(stream_key, "secret?bandwidthtest=true");
            assert!(props.iter().any(|(k, _)| k == "fourCcList"));
        }
        other => panic!("unexpected events {other:?}"),
    }
    assert!(!c.is_publishing());
    s.accept_publish();
    let (ce, _) = pump(&mut c, &mut s);
    assert_eq!(ce, vec![ClientEvent::Publishing]);
    (c, s)
}

#[test]
fn publish_flow_and_media() {
    let (mut c, mut s) = connected_pair();
    let meta = crate::amf0::encode_all(&[
        Amf0Value::string("@setDataFrame"),
        Amf0Value::string("onMetaData"),
        Amf0Value::EcmaArray(vec![("width".into(), Amf0Value::Number(1280.0))]),
    ]);
    c.send_data(0, &meta);
    let big = vec![0x17u8; 20_000];
    c.send_media(MediaKind::Video, 0x0100_0000, &big);
    c.send_media(MediaKind::Audio, 23, &[0xaf, 0x01, 1, 2]);
    let (_, se) = pump(&mut c, &mut s);
    assert_eq!(
        se,
        vec![
            ServerEvent::Metadata {
                timestamp: 0,
                payload: meta.freeze()
            },
            ServerEvent::Media {
                kind: MediaKind::Video,
                timestamp: 0x0100_0000,
                payload: Bytes::from(big)
            },
            ServerEvent::Media {
                kind: MediaKind::Audio,
                timestamp: 23,
                payload: Bytes::from_static(&[0xaf, 0x01, 1, 2])
            },
        ]
    );
    c.close();
    let (_, se) = pump(&mut c, &mut s);
    assert_eq!(se, vec![ServerEvent::Unpublish]);
}

#[test]
fn rejected_publish_reports_error() {
    let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://x/live", "bad"));
    let mut s = ServerSession::new(ServerConfig::default());
    pump(&mut c, &mut s);
    s.reject_publish("NetStream.Publish.BadName", "wrong key");
    let (ce, _) = pump(&mut c, &mut s);
    assert_eq!(
        ce,
        vec![ClientEvent::Error {
            code: "NetStream.Publish.BadName".into(),
            description: "wrong key".into()
        }]
    );
}

#[test]
fn media_before_publish_is_ignored() {
    let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://x/live", "k"));
    c.send_media(MediaKind::Video, 0, &[1, 2, 3]);
    let mut s = ServerSession::new(ServerConfig::default());
    let (_, se) = pump(&mut c, &mut s);
    assert!(se.iter().all(|e| !matches!(e, ServerEvent::Media { .. })));
}

#[test]
fn acknowledgements_are_sent_after_window() {
    let (mut c, mut s) = connected_pair();
    // Server announced a 2.5 MB window; send more than that.
    for i in 0..30 {
        c.send_media(MediaKind::Video, i, &vec![0u8; 100_000]);
    }
    let to_s = c.take_output();
    s.feed(&to_s).unwrap();
    let back = s.take_output();
    // An Acknowledgement is a 16-byte message on csid 2 (12-byte header + 4-byte payload).
    let mut dec = crate::chunk::ChunkDecoder::new();
    dec.set_chunk_size(4096).unwrap();
    dec.push(&back);
    let m = dec.next_message().unwrap().expect("ack");
    assert_eq!(m.type_id, crate::message::ACKNOWLEDGEMENT);
}

mod arbitrary_input {
    use proptest::prelude::*;

    use crate::handshake::{ClientHandshake, ServerHandshake};
    use crate::session::*;

    proptest! {
        /// Whatever a peer sends, sessions and handshakes return errors, never panic.
        #[test]
        fn sessions_never_panic(data in prop::collection::vec(any::<u8>(), 0..4096)) {
            let mut s = ServerSession::new(ServerConfig::default());
            let _ = s.feed(&data);
            s.accept_publish();
            let _ = s.feed(&data);
            let mut c = ClientSession::new(ClientConfig::new("app", "rtmp://x/app", "k"));
            let _ = c.feed(&data);
            let mut out = bytes::BytesMut::new();
            let _ = ServerHandshake::new().feed(&data, &mut out);
            let mut h = ClientHandshake::new();
            h.start(&mut out);
            let _ = h.feed(&data, &mut out);
        }

        /// A valid connect followed by garbage still never panics.
        #[test]
        fn garbage_after_connect_never_panics(data in prop::collection::vec(any::<u8>(), 0..2048)) {
            let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://x/live", "k"));
            let mut s = ServerSession::new(ServerConfig::default());
            let hello = c.take_output();
            let _ = s.feed(&hello);
            let _ = s.feed(&data);
        }
    }
}

#[test]
fn large_messages_are_refused_before_publishing() {
    // An unauthenticated peer declares a huge AMF0 command (a 16 MiB strict array
    // of nulls would take about 800 MiB to decode). The header alone is refused.
    let mut s = ServerSession::new(ServerConfig::default());
    let mut enc = crate::chunk::ChunkEncoder::new();
    enc.set_chunk_size(1 << 20);
    let mut out = bytes::BytesMut::new();
    let payload = vec![0x05u8; MAX_PRE_PUBLISH_MESSAGE + 1];
    enc.write(&mut out, CSID_COMMAND, 0, COMMAND_AMF0, 0, &payload);
    let err = s.feed(&out[..16]).unwrap_err();
    assert!(
        matches!(
            err,
            SessionError::Chunk(crate::chunk::ChunkError::MessageTooLarge { .. })
        ),
        "{err}"
    );
}

#[test]
fn publishing_allows_large_media_but_bounds_other_messages() {
    let (mut c, mut s) = connected_pair();
    // A 4 MB keyframe is fine once the publish was accepted.
    let key = vec![0x17u8; 4 << 20];
    c.send_media(MediaKind::Video, 0, &key);
    let (_, se) = pump(&mut c, &mut s);
    assert!(
        se.iter()
            .any(|e| matches!(e, ServerEvent::Media { payload, .. } if payload.len() == key.len()))
    );
    // Data messages stay bounded.
    c.send_data(0, &vec![0x05u8; MAX_NON_MEDIA_MESSAGE + 1]);
    assert!(s.feed(&c.take_output()).is_err());
}

#[test]
fn a_second_connect_is_refused() {
    // Each one is answered and reported: anyone who can reach the ingest could
    // otherwise send them without end, before giving any stream key.
    let (_c, mut s) = connected_pair();
    let mut out = bytes::BytesMut::new();
    crate::message::write_command(
        &crate::chunk::ChunkEncoder::new(),
        &mut out,
        crate::message::CSID_COMMAND,
        0,
        &[
            Amf0Value::string("connect"),
            Amf0Value::Number(1.0),
            Amf0Value::object([("app", Amf0Value::string("live"))]),
        ],
    );
    assert!(matches!(s.feed(&out), Err(SessionError::Protocol(_))));
}
