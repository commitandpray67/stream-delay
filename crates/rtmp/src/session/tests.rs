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
    assert!(!c.is_publishing() && !s.is_publishing());
    assert_eq!(s.app(), "live");
    assert_eq!(s.stream_key(), "secret?bandwidthtest=true");
    s.accept_publish();
    let (ce, _) = pump(&mut c, &mut s);
    assert_eq!(ce, vec![ClientEvent::Publishing]);
    assert!(c.is_publishing() && s.is_publishing());
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
fn nothing_but_commands_is_taken_before_the_publish_is_accepted() {
    // Our client sends no media before publishing; a peer can.
    let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://x/live", "k"));
    let mut s = ServerSession::new(ServerConfig::default());
    pump(&mut c, &mut s);
    let enc = crate::chunk::ChunkEncoder::new();
    let raw = |enc: &crate::chunk::ChunkEncoder| {
        let mut out = bytes::BytesMut::new();
        let meta = crate::amf0::encode_all(&[
            Amf0Value::string("@setDataFrame"),
            Amf0Value::string("onMetaData"),
            Amf0Value::EcmaArray(vec![]),
        ]);
        let caption = crate::amf0::encode_all(&[Amf0Value::string("onCaptionInfo")]);
        enc.write(&mut out, 20, 0, VIDEO, 1, &[0x17, 1, 0, 0, 0]);
        enc.write(&mut out, 21, 0, AUDIO, 1, &[0xaf, 1, 0]);
        enc.write(&mut out, 22, 0, DATA_AMF0, 1, &meta);
        enc.write(&mut out, 23, 0, DATA_AMF0, 1, &caption);
        out
    };
    assert_eq!(s.feed(&raw(&enc)).unwrap(), vec![]);
    s.accept_publish();
    assert_eq!(s.feed(&raw(&enc)).unwrap().len(), 4);
}

#[test]
fn metadata_without_set_data_frame_gets_it() {
    let (mut c, mut s) = connected_pair();
    let meta = crate::amf0::encode_all(&[
        Amf0Value::string("onMetaData"),
        Amf0Value::EcmaArray(vec![("width".into(), Amf0Value::Number(1280.0))]),
    ]);
    c.send_data(0, &meta);
    let (_, se) = pump(&mut c, &mut s);
    let [ServerEvent::Metadata { payload, .. }] = &se[..] else {
        panic!("{se:?}");
    };
    let values = crate::amf0::decode_all(payload).unwrap();
    assert_eq!(values[0].as_str(), Some("@setDataFrame"));
    assert_eq!(values[1].as_str(), Some("onMetaData"));
}

#[test]
fn commands_and_metadata_up_to_their_limits_are_taken() {
    // A connect command of 30 KB, under the 64 KiB allowed before publishing.
    let mut cfg = ClientConfig::new("live", "rtmp://127.0.0.1/live", "k");
    cfg.extra_connect_props = vec![("note".into(), Amf0Value::string("x".repeat(30_000)))];
    let mut c = ClientSession::new(cfg);
    let mut s = ServerSession::new(ServerConfig::default());
    let (_, se) = pump(&mut c, &mut s);
    assert!(matches!(se[0], ServerEvent::Connect { .. }), "{se:?}");
    // Metadata of 500 KB once publishing, under the 1 MiB allowed.
    let (mut c, mut s) = connected_pair();
    let meta = crate::amf0::encode_all(&[
        Amf0Value::string("@setDataFrame"),
        Amf0Value::string("onMetaData"),
        Amf0Value::EcmaArray(vec![("x".into(), Amf0Value::string("y".repeat(500_000)))]),
    ]);
    c.send_data(0, &meta);
    let (_, se) = pump(&mut c, &mut s);
    assert!(matches!(se[..], [ServerEvent::Metadata { .. }]), "{se:?}");
}

#[test]
fn received_media_is_kept_in_the_shared_pool() {
    let pool = crate::ArenaPool::new(64 << 20);
    let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://127.0.0.1/live", "k"));
    let mut s = ServerSession::new(ServerConfig::default());
    s.set_arena_pool(pool.clone());
    pump(&mut c, &mut s);
    s.accept_publish();
    pump(&mut c, &mut s);
    c.send_media(MediaKind::Video, 0, &[0x17; 10_000]);
    let (_, se) = pump(&mut c, &mut s);
    assert!(pool.bytes_in_use() >= 10_000);
    drop(se);
}

/// Reads what one side sends, from its first byte, as the other side would.
struct Tap(crate::chunk::ChunkDecoder);

impl Tap {
    fn new() -> Self {
        Self(crate::chunk::ChunkDecoder::new())
    }

    fn read(&mut self, bytes: &[u8]) -> Vec<Message> {
        self.0.push(bytes);
        let mut out = Vec::new();
        while let Some(m) = self.0.next_message().unwrap() {
            if m.type_id == SET_CHUNK_SIZE {
                self.0
                    .set_chunk_size(read_u32(&m.payload).unwrap())
                    .unwrap();
            }
            out.push(m);
        }
        out
    }
}

/// A message in a few words: a command's name and transaction id, or a control
/// message's name and value.
fn summary(m: &Message) -> String {
    let u32_at = |i: usize| read_u32(&m.payload[i..]).unwrap();
    match m.type_id {
        COMMAND_AMF0 => {
            let v = crate::amf0::decode_all(&m.payload).unwrap();
            format!("{} {}", v[0].as_str().unwrap(), v[1].as_number().unwrap())
        }
        SET_CHUNK_SIZE => format!("chunk size {}", u32_at(0)),
        WINDOW_ACK_SIZE => format!("window {}", u32_at(0)),
        SET_PEER_BANDWIDTH => format!("peer bandwidth {} {}", u32_at(0), m.payload[4]),
        USER_CONTROL => {
            let (event, value) = read_user_control(&m.payload).unwrap();
            format!("user control {event} {value}")
        }
        t => format!("type {t}"),
    }
}

#[test]
fn a_publish_goes_the_way_librtmp_expects() {
    let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://127.0.0.1/live", "k"));
    let mut s = ServerSession::new(ServerConfig::default());
    let (mut from_c, mut from_s) = (Tap::new(), Tap::new());
    let (mut sent_c, mut sent_s) = (Vec::new(), Vec::new());
    loop {
        let to_s = c.take_output();
        let to_c = s.take_output();
        if to_s.is_empty() && to_c.is_empty() {
            break;
        }
        sent_c.extend(from_c.read(&to_s));
        sent_s.extend(from_s.read(&to_c));
        for e in s.feed(&to_s).unwrap() {
            if matches!(e, ServerEvent::PublishRequest { .. }) {
                s.accept_publish();
            }
        }
        c.feed(&to_c).unwrap();
    }
    assert!(c.is_publishing());
    let sent_c: Vec<_> = sent_c.iter().map(summary).collect();
    let sent_s: Vec<_> = sent_s.iter().map(summary).collect();
    assert_eq!(
        sent_c,
        [
            "chunk size 4096",
            "connect 1",
            // The answer to the server's Set Peer Bandwidth.
            "window 2500000",
            "releaseStream 2",
            "FCPublish 3",
            "createStream 4",
            "publish 5",
        ]
    );
    assert_eq!(
        sent_s,
        [
            "window 2500000",
            "peer bandwidth 2500000 2",
            "chunk size 4096",
            "_result 1",
            "_result 2",
            "onFCPublish 0",
            "_result 3",
            "_result 4",
            // Stream Begin for stream 1.
            "user control 0 1",
            "onStatus 0",
        ]
    );
}

/// An ingest server played by hand, to answer the client in ways ours never does.
struct Script {
    enc: crate::chunk::ChunkEncoder,
    tap: Tap,
}

impl Script {
    /// A client that has sent `connect`, and the server it talks to.
    fn start() -> (ClientSession, Script) {
        let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://x/live", "k"));
        let mut script = Script {
            enc: crate::chunk::ChunkEncoder::new(),
            tap: Tap::new(),
        };
        assert_eq!(script.heard(&mut c), ["chunk size 4096", "connect 1"]);
        (c, script)
    }

    fn say(&self, c: &mut ClientSession, values: &[Amf0Value]) -> Vec<ClientEvent> {
        let mut out = bytes::BytesMut::new();
        write_command(&self.enc, &mut out, CSID_COMMAND, 0, values);
        c.feed(&out).unwrap()
    }

    fn heard_messages(&mut self, c: &mut ClientSession) -> Vec<Message> {
        self.tap.read(&c.take_output())
    }

    fn heard(&mut self, c: &mut ClientSession) -> Vec<String> {
        self.heard_messages(c).iter().map(summary).collect()
    }
}

fn reply(name: &str, txid: f64, info: Amf0Value) -> [Amf0Value; 4] {
    [
        Amf0Value::string(name),
        Amf0Value::Number(txid),
        Amf0Value::Null,
        info,
    ]
}

fn status(code: &str) -> Amf0Value {
    crate::message::status_object("status", code, "")
}

fn error_code(events: &[ClientEvent]) -> Option<&str> {
    match events {
        [ClientEvent::Error { code, .. }] => Some(code),
        _ => None,
    }
}

#[test]
fn the_client_follows_what_the_server_answers() {
    // Connect refused in its result, or with an error.
    let (mut c, script) = Script::start();
    let e = script.say(
        &mut c,
        &reply("_result", 1.0, status("NetConnection.Connect.Rejected")),
    );
    assert_eq!(error_code(&e), Some("NetConnection.Connect.Rejected"));
    let (mut c, script) = Script::start();
    let e = script.say(
        &mut c,
        &reply("_error", 1.0, status("NetConnection.Connect.Failed")),
    );
    assert_eq!(error_code(&e), Some("NetConnection.Connect.Failed"));

    let (mut c, mut script) = Script::start();
    // With a description of 50 KB: server messages may be up to 128 KiB.
    let ok = reply(
        "_result",
        1.0,
        crate::message::status_object(
            "status",
            "NetConnection.Connect.Success",
            &"x".repeat(50_000),
        ),
    );
    assert_eq!(script.say(&mut c, &ok), []);
    assert_eq!(
        script.heard(&mut c),
        ["releaseStream 2", "FCPublish 3", "createStream 4"]
    );
    // Servers without releaseStream or FCPublish answer them with errors.
    for txid in [2.0, 3.0] {
        let e = script.say(
            &mut c,
            &reply("_error", txid, status("NetConnection.Call.Failed")),
        );
        assert_eq!(e, []);
    }
    // A second answer to connect changes nothing.
    script.say(&mut c, &ok);
    assert_eq!(script.heard(&mut c), Vec::<String>::new());
    // Published on the stream the server created.
    script.say(&mut c, &reply("_result", 4.0, Amf0Value::Number(7.0)));
    let heard = script.heard_messages(&mut c);
    assert_eq!(heard.iter().map(summary).collect::<Vec<_>>(), ["publish 5"]);
    assert_eq!(heard[0].stream_id, 7);
    // Publish.Start, sent as an AMF3 command (a format byte, then AMF0).
    let mut body = vec![0u8];
    body.extend_from_slice(&crate::amf0::encode_all(&reply(
        "onStatus",
        0.0,
        status("NetStream.Publish.Start"),
    )));
    let mut out = bytes::BytesMut::new();
    script
        .enc
        .write(&mut out, CSID_COMMAND, 0, COMMAND_AMF3, 7, &body);
    assert_eq!(c.feed(&out).unwrap(), [ClientEvent::Publishing]);
    assert!(c.is_publishing());
    // The server closing the connection.
    let close = [
        Amf0Value::string("close"),
        Amf0Value::Number(0.0),
        Amf0Value::Null,
    ];
    assert_eq!(
        error_code(&script.say(&mut c, &close)),
        Some("NetConnection.Close")
    );

    // An error answering createStream or publish ends it.
    for txid in [4.0, 5.0] {
        let (mut c, mut script) = Script::start();
        script.say(&mut c, &ok);
        if txid == 5.0 {
            script.say(&mut c, &reply("_result", 4.0, Amf0Value::Number(1.0)));
        }
        script.heard(&mut c);
        let e = script.say(
            &mut c,
            &reply("_error", txid, status("NetStream.Publish.BadName")),
        );
        assert_eq!(error_code(&e), Some("NetStream.Publish.BadName"), "{txid}");
    }
}

#[test]
fn a_client_config_never_shows_its_key() {
    let cfg = ClientConfig::new("app", "rtmps://live.twitch.tv/app", "live_123_secret");
    let shown = format!("{cfg:?}");
    assert!(!shown.contains("live_123_secret"), "{shown}");
    assert!(shown.contains("rtmps://live.twitch.tv/app"), "{shown}");
}

#[test]
fn a_command_sent_as_amf3_is_read_too() {
    let mut s = ServerSession::new(ServerConfig::default());
    let mut body = vec![0u8];
    body.extend_from_slice(&crate::amf0::encode_all(&[
        Amf0Value::string("connect"),
        Amf0Value::Number(1.0),
        Amf0Value::object([("app", Amf0Value::string("live"))]),
    ]));
    let mut out = bytes::BytesMut::new();
    crate::chunk::ChunkEncoder::new().write(&mut out, CSID_COMMAND, 0, COMMAND_AMF3, 0, &body);
    let e = s.feed(&out).unwrap();
    assert!(
        matches!(&e[..], [ServerEvent::Connect { app, .. }] if app == "live"),
        "{e:?}"
    );
}

#[test]
fn the_link_answers_pings_and_follows_the_peers_window() {
    let mut link = Link::new(2_500_000);
    let mut from_link = Tap::new();
    let control = |type_id, payload: &[u8]| Message {
        csid: CSID_CONTROL,
        timestamp: 0,
        type_id,
        stream_id: 0,
        payload: Bytes::copy_from_slice(payload),
    };
    // A ping is answered with its own value. Servers drop publishers that do not.
    assert!(
        link.handle_control(&control(USER_CONTROL, &[0, 6, 0, 0, 0x30, 0x39]))
            .unwrap()
    );
    // Set Peer Bandwidth is answered with a window, as librtmp does.
    assert!(
        link.handle_control(&control(SET_PEER_BANDWIDTH, &[0, 0, 0x27, 0x10, 2]))
            .unwrap()
    );
    // Acknowledgements need no answer.
    assert!(
        link.handle_control(&control(ACKNOWLEDGEMENT, &[0, 0, 0, 1]))
            .unwrap()
    );
    let sent: Vec<_> = from_link
        .read(&link.take_output())
        .iter()
        .map(summary)
        .collect();
    assert_eq!(sent, ["user control 7 12345", "window 10000"]);
    // The peer's window: acknowledge every 500 bytes.
    assert!(
        link.handle_control(&control(WINDOW_ACK_SIZE, &[0, 0, 0x01, 0xf4]))
            .unwrap()
    );
    link.received(499);
    assert!(link.take_output().is_empty());
    link.received(1);
    let sent: Vec<_> = from_link
        .read(&link.take_output())
        .iter()
        .map(summary)
        .collect();
    assert_eq!(sent, ["type 3"]);
    // A window of 0: no acknowledgements at all.
    assert!(
        link.handle_control(&control(WINDOW_ACK_SIZE, &[0, 0, 0, 0]))
            .unwrap()
    );
    link.received(10_000_000);
    assert!(link.take_output().is_empty());
}

#[test]
fn an_aborted_message_is_dropped() {
    let mut link = Link::new(2_500_000);
    // The first 128-byte chunk of a 300-byte command on chunk stream 3.
    let mut first = vec![0x03, 0, 0, 0, 0, 0x01, 0x2c, COMMAND_AMF0, 0, 0, 0, 0];
    first.extend([b'a'; 128]);
    link.decoder.push(&first);
    assert!(link.decoder.next_message().unwrap().is_none());
    let abort = Message {
        csid: CSID_CONTROL,
        timestamp: 0,
        type_id: ABORT,
        stream_id: 0,
        payload: Bytes::from_static(&[0, 0, 0, 3]),
    };
    assert!(link.handle_control(&abort).unwrap());
    // Another message of the same size, in chunks with type 3 headers only.
    for n in [128, 128, 44] {
        let mut chunk = vec![0xc3];
        chunk.extend(std::iter::repeat_n(b'b', n));
        link.decoder.push(&chunk);
    }
    let m = link.decoder.next_message().unwrap().unwrap();
    assert_eq!(&m.payload[..], &[b'b'; 300][..]);
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

#[test]
fn a_stream_id_the_server_cannot_mean_ends_the_connection() {
    let ok = reply(
        "_result",
        1.0,
        crate::message::status_object("status", "NetConnection.Connect.Success", ""),
    );
    for (id, valid) in [
        (1.0, true),
        (f64::from(u32::MAX), true),
        (0.0, false),
        (-1.0, false),
        (1.5, false),
        (f64::NAN, false),
        (f64::INFINITY, false),
        (4_294_967_296.0, false),
    ] {
        let (mut c, mut script) = Script::start();
        script.say(&mut c, &ok);
        script.heard(&mut c);
        let mut out = bytes::BytesMut::new();
        write_command(
            &script.enc,
            &mut out,
            CSID_COMMAND,
            0,
            &reply("_result", 4.0, Amf0Value::Number(id)),
        );
        let fed = c.feed(&out);
        assert_eq!(fed.is_ok(), valid, "{id}: {fed:?}");
        if valid {
            assert!(
                script
                    .heard(&mut c)
                    .iter()
                    .any(|m| m.starts_with("publish"))
            );
        }
    }
}

#[test]
fn a_second_publish_says_so() {
    let (mut c, mut s) = connected_pair();
    let mut out = bytes::BytesMut::new();
    write_command(
        &crate::chunk::ChunkEncoder::new(),
        &mut out,
        CSID_STREAM_COMMAND,
        1,
        &[
            Amf0Value::string("publish"),
            Amf0Value::Number(9.0),
            Amf0Value::Null,
            Amf0Value::string("again"),
            Amf0Value::string("live"),
        ],
    );
    c.take_output();
    let err = s.feed(&out).unwrap_err();
    assert!(err.to_string().contains("publish sent twice"), "{err}");
}
