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
