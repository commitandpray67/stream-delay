use bytes::Bytes;

use super::{Link, MAX_NON_MEDIA_MESSAGE, MAX_PRE_PUBLISH_MESSAGE, MediaKind, SessionError};
use crate::amf0::{self, Amf0Value};
use crate::chunk::Message;
use crate::message::{self, *};

/// Settings announced to the publisher.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub chunk_size: u32,
    pub window_ack_size: u32,
    pub peer_bandwidth: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            chunk_size: 4096,
            window_ack_size: 2_500_000,
            peer_bandwidth: 2_500_000,
        }
    }
}

/// Something the application needs to know about.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// The client sent `connect`. `props` is the full command object.
    Connect {
        app: String,
        tc_url: Option<String>,
        props: Vec<(String, Amf0Value)>,
    },
    /// The client wants to publish. Answer with `accept_publish` or `reject_publish`.
    PublishRequest { app: String, stream_key: String },
    Media {
        kind: MediaKind,
        timestamp: u32,
        payload: Bytes,
    },
    /// Stream metadata; the payload always starts with `@setDataFrame`.
    Metadata { timestamp: u32, payload: Bytes },
    /// Any other AMF0 data message (captions, cue points).
    Data { timestamp: u32, payload: Bytes },
    /// The client stopped publishing (FCUnpublish, deleteStream or closeStream).
    Unpublish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitConnect,
    Connected,
    PublishPending,
    Publishing,
    Closed,
}

/// Server side of a publish session (what OBS talks to).
pub struct ServerSession {
    link: Link,
    config: ServerConfig,
    state: State,
    app: String,
    stream_key: String,
}

const STREAM_ID: u32 = 1;

impl ServerSession {
    pub fn new(config: ServerConfig) -> Self {
        let mut link = Link::new(config.window_ack_size);
        link.decoder
            .set_max_message_len(MAX_PRE_PUBLISH_MESSAGE, MAX_PRE_PUBLISH_MESSAGE);
        Self {
            link,
            config,
            state: State::AwaitConnect,
            app: String::new(),
            stream_key: String::new(),
        }
    }

    /// Copies received messages into blocks from `pool`, shared with other
    /// sessions; see [`crate::ArenaPool`].
    pub fn set_arena_pool(&mut self, pool: crate::ArenaPool) {
        self.link.decoder.set_arena_pool(pool);
    }

    pub fn app(&self) -> &str {
        &self.app
    }

    pub fn stream_key(&self) -> &str {
        &self.stream_key
    }

    pub fn is_publishing(&self) -> bool {
        self.state == State::Publishing
    }

    /// Feeds bytes received after the handshake.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<ServerEvent>, SessionError> {
        self.link.received(data.len());
        self.link.decoder.push(data);
        let mut events = Vec::new();
        while let Some(m) = self.link.decoder.next_message()? {
            self.handle(m, &mut events)?;
        }
        Ok(events)
    }

    pub fn take_output(&mut self) -> Bytes {
        self.link.take_output()
    }

    pub fn accept_publish(&mut self) {
        if self.state != State::PublishPending {
            return;
        }
        self.state = State::Publishing;
        // The publisher is authenticated now: allow full-size media.
        self.link
            .decoder
            .set_max_message_len(usize::MAX, MAX_NON_MEDIA_MESSAGE);
        let enc = &self.link.encoder;
        let out = &mut self.link.out;
        write_user_control(enc, out, UC_STREAM_BEGIN, STREAM_ID);
        write_command(
            enc,
            out,
            CSID_STREAM_COMMAND,
            STREAM_ID,
            &[
                Amf0Value::string("onStatus"),
                Amf0Value::Number(0.0),
                Amf0Value::Null,
                status_object(
                    "status",
                    "NetStream.Publish.Start",
                    &format!("{} is now published.", self.app),
                ),
            ],
        );
    }

    pub fn reject_publish(&mut self, code: &str, description: &str) {
        self.state = State::Closed;
        write_command(
            &self.link.encoder,
            &mut self.link.out,
            CSID_STREAM_COMMAND,
            STREAM_ID,
            &[
                Amf0Value::string("onStatus"),
                Amf0Value::Number(0.0),
                Amf0Value::Null,
                status_object("error", code, description),
            ],
        );
    }

    fn reply_result(&mut self, txid: f64, values: Vec<Amf0Value>) {
        let mut v = vec![Amf0Value::string("_result"), Amf0Value::Number(txid)];
        v.extend(values);
        write_command(&self.link.encoder, &mut self.link.out, CSID_COMMAND, 0, &v);
    }

    fn handle(&mut self, m: Message, events: &mut Vec<ServerEvent>) -> Result<(), SessionError> {
        if self.link.handle_control(&m)? {
            return Ok(());
        }
        match m.type_id {
            COMMAND_AMF0 => self.handle_command(&m.payload, events),
            COMMAND_AMF3 => {
                // AMF3 commands carry an AMF0 body after a leading format byte.
                self.handle_command(m.payload.get(1..).unwrap_or_default(), events)
            }
            AUDIO | VIDEO if self.state == State::Publishing => {
                if !m.payload.is_empty() {
                    let kind = if m.type_id == AUDIO {
                        MediaKind::Audio
                    } else {
                        MediaKind::Video
                    };
                    events.push(ServerEvent::Media {
                        kind,
                        timestamp: m.timestamp,
                        payload: m.payload,
                    });
                }
                Ok(())
            }
            DATA_AMF0 if self.state == State::Publishing => {
                let (first, used) = match amf0::decode_one(&m.payload) {
                    Ok(v) => v,
                    Err(_) => return Ok(()),
                };
                let is_meta = match first.as_str() {
                    Some("@setDataFrame") => amf0::decode_one(&m.payload[used..])
                        .map(|(v, _)| v.as_str() == Some("onMetaData"))
                        .unwrap_or(false),
                    Some("onMetaData") => true,
                    _ => false,
                };
                if is_meta {
                    if let Some(p) = message::with_set_data_frame(&m.payload) {
                        events.push(ServerEvent::Metadata {
                            timestamp: m.timestamp,
                            payload: p.freeze(),
                        });
                    }
                } else {
                    events.push(ServerEvent::Data {
                        timestamp: m.timestamp,
                        payload: m.payload,
                    });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn handle_command(
        &mut self,
        payload: &[u8],
        events: &mut Vec<ServerEvent>,
    ) -> Result<(), SessionError> {
        let values = amf0::decode_all(payload)?;
        let name = values
            .first()
            .and_then(Amf0Value::as_str)
            .unwrap_or_default()
            .to_string();
        let txid = values.get(1).and_then(Amf0Value::as_number).unwrap_or(0.0);
        match name.as_str() {
            "connect" => {
                // Once per connection: each is answered and reported.
                if self.state != State::AwaitConnect {
                    return Err(SessionError::Protocol("connect sent twice".into()));
                }
                let obj = values.get(2).cloned().unwrap_or(Amf0Value::Null);
                self.app = obj
                    .get("app")
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tc_url = obj
                    .get("tcUrl")
                    .and_then(Amf0Value::as_str)
                    .map(str::to_string);
                let enc = &self.link.encoder;
                let out = &mut self.link.out;
                write_u32_control(enc, out, WINDOW_ACK_SIZE, self.config.window_ack_size);
                write_set_peer_bandwidth(enc, out, self.config.peer_bandwidth, 2);
                self.link.set_out_chunk_size(self.config.chunk_size);
                let enc_obj = obj.get("objectEncoding").and_then(Amf0Value::as_number);
                self.reply_result(
                    txid,
                    vec![
                        Amf0Value::object([
                            ("fmsVer", Amf0Value::string("FMS/3,0,1,123")),
                            ("capabilities", Amf0Value::Number(31.0)),
                        ]),
                        Amf0Value::object([
                            ("level", Amf0Value::string("status")),
                            ("code", Amf0Value::string("NetConnection.Connect.Success")),
                            ("description", Amf0Value::string("Connection succeeded.")),
                            ("objectEncoding", Amf0Value::Number(enc_obj.unwrap_or(0.0))),
                        ]),
                    ],
                );
                self.state = State::Connected;
                events.push(ServerEvent::Connect {
                    app: self.app.clone(),
                    tc_url,
                    props: obj.props().map(<[_]>::to_vec).unwrap_or_default(),
                });
            }
            "releaseStream" | "FCPublish" => {
                if name == "FCPublish" {
                    let key = values.get(3).and_then(Amf0Value::as_str).unwrap_or("");
                    write_command(
                        &self.link.encoder,
                        &mut self.link.out,
                        CSID_COMMAND,
                        0,
                        &[
                            Amf0Value::string("onFCPublish"),
                            Amf0Value::Number(0.0),
                            Amf0Value::Null,
                            Amf0Value::object([
                                ("code", Amf0Value::string("NetStream.Publish.Start")),
                                ("description", Amf0Value::string(key)),
                            ]),
                        ],
                    );
                }
                self.reply_result(txid, vec![Amf0Value::Null, Amf0Value::Undefined]);
            }
            "createStream" => {
                self.reply_result(
                    txid,
                    vec![Amf0Value::Null, Amf0Value::Number(STREAM_ID as f64)],
                );
            }
            "publish" => {
                if self.state != State::Connected {
                    return Err(SessionError::Protocol("publish before connect".into()));
                }
                self.stream_key = values
                    .get(3)
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.state = State::PublishPending;
                events.push(ServerEvent::PublishRequest {
                    app: self.app.clone(),
                    stream_key: self.stream_key.clone(),
                });
            }
            "FCUnpublish" | "deleteStream" | "closeStream" => {
                if matches!(self.state, State::Publishing | State::PublishPending) {
                    self.state = State::Closed;
                    events.push(ServerEvent::Unpublish);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
