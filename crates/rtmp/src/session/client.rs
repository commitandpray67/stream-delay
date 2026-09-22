use std::fmt;

use bytes::Bytes;

use super::{Link, MediaKind, SessionError};
use crate::amf0::{self, Amf0Value};
use crate::chunk::Message;
use crate::message::*;

/// What to publish, and where.
#[derive(Clone)]
pub struct ClientConfig {
    pub app: String,
    pub tc_url: String,
    pub stream_key: String,
    /// Extra `connect` properties to mirror from the encoder (for example the Enhanced
    /// RTMP `fourCcList`). Properties we set ourselves are not overridden.
    pub extra_connect_props: Vec<(String, Amf0Value)>,
    pub chunk_size: u32,
    pub flash_ver: String,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("tc_url", &self.tc_url)
            .field("stream_key", &"<redacted>")
            .finish()
    }
}

impl ClientConfig {
    pub fn new(app: impl Into<String>, tc_url: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            tc_url: tc_url.into(),
            stream_key: key.into(),
            extra_connect_props: Vec::new(),
            chunk_size: 4096,
            flash_ver: "FMLE/3.0 (compatible; FMSc/1.0)".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The server accepted the stream; media can be sent now.
    Publishing,
    /// The server refused or closed the stream.
    Error { code: String, description: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting,
    CreatingStream,
    PublishPending,
    Publishing,
    Failed,
    Closed,
}

const TX_CONNECT: f64 = 1.0;
const TX_RELEASE: f64 = 2.0;
const TX_FCPUBLISH: f64 = 3.0;
const TX_CREATE: f64 = 4.0;
const TX_PUBLISH: f64 = 5.0;

/// Client side of a publish session (what we use to talk to Twitch).
pub struct ClientSession {
    link: Link,
    config: ClientConfig,
    state: State,
    stream_id: u32,
}

impl ClientSession {
    /// Creates the session and queues `connect`. Call after the handshake.
    pub fn new(config: ClientConfig) -> Self {
        let mut s = Self {
            link: Link::new(2_500_000),
            config,
            state: State::Connecting,
            stream_id: 0,
        };
        s.link.set_out_chunk_size(s.config.chunk_size);
        let mut props: Vec<(String, Amf0Value)> = vec![
            ("app".into(), Amf0Value::string(s.config.app.clone())),
            ("type".into(), Amf0Value::string("nonprivate")),
            (
                "flashVer".into(),
                Amf0Value::string(s.config.flash_ver.clone()),
            ),
            ("swfUrl".into(), Amf0Value::string(s.config.tc_url.clone())),
            ("tcUrl".into(), Amf0Value::string(s.config.tc_url.clone())),
        ];
        for (k, v) in &s.config.extra_connect_props {
            if !props.iter().any(|(pk, _)| pk == k) {
                props.push((k.clone(), v.clone()));
            }
        }
        write_command(
            &s.link.encoder,
            &mut s.link.out,
            CSID_COMMAND,
            0,
            &[
                Amf0Value::string("connect"),
                Amf0Value::Number(TX_CONNECT),
                Amf0Value::Object(props),
            ],
        );
        s
    }

    pub fn is_publishing(&self) -> bool {
        self.state == State::Publishing
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<ClientEvent>, SessionError> {
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

    /// Queues an audio or video message. Ignored unless publishing.
    pub fn send_media(&mut self, kind: MediaKind, timestamp: u32, payload: &[u8]) {
        if self.state != State::Publishing {
            return;
        }
        let (csid, ty) = match kind {
            MediaKind::Audio => (CSID_AUDIO, AUDIO),
            MediaKind::Video => (CSID_VIDEO, VIDEO),
        };
        self.link.encoder.write(
            &mut self.link.out,
            csid,
            timestamp,
            ty,
            self.stream_id,
            payload,
        );
    }

    /// Queues an AMF0 data message (metadata, captions). Ignored unless publishing.
    pub fn send_data(&mut self, timestamp: u32, payload: &[u8]) {
        if self.state != State::Publishing {
            return;
        }
        self.link.encoder.write(
            &mut self.link.out,
            CSID_DATA,
            timestamp,
            DATA_AMF0,
            self.stream_id,
            payload,
        );
    }

    /// Queues a clean shutdown (FCUnpublish then deleteStream).
    pub fn close(&mut self) {
        if matches!(self.state, State::Closed | State::Failed) {
            return;
        }
        if self.stream_id != 0 {
            let enc = &self.link.encoder;
            let out = &mut self.link.out;
            write_command(
                enc,
                out,
                CSID_COMMAND,
                0,
                &[
                    Amf0Value::string("FCUnpublish"),
                    Amf0Value::Number(6.0),
                    Amf0Value::Null,
                    Amf0Value::string(self.config.stream_key.clone()),
                ],
            );
            write_command(
                enc,
                out,
                CSID_COMMAND,
                0,
                &[
                    Amf0Value::string("deleteStream"),
                    Amf0Value::Number(7.0),
                    Amf0Value::Null,
                    Amf0Value::Number(self.stream_id as f64),
                ],
            );
        }
        self.state = State::Closed;
    }

    fn fail(&mut self, events: &mut Vec<ClientEvent>, code: &str, description: &str) {
        self.state = State::Failed;
        events.push(ClientEvent::Error {
            code: code.into(),
            description: description.into(),
        });
    }

    fn handle(&mut self, m: Message, events: &mut Vec<ClientEvent>) -> Result<(), SessionError> {
        if self.link.handle_control(&m)? {
            return Ok(());
        }
        let body: &[u8] = match m.type_id {
            COMMAND_AMF0 => &m.payload,
            COMMAND_AMF3 => m.payload.get(1..).unwrap_or_default(),
            _ => return Ok(()),
        };
        let values = amf0::decode_all(body)?;
        let name = values
            .first()
            .and_then(Amf0Value::as_str)
            .unwrap_or_default();
        let txid = values.get(1).and_then(Amf0Value::as_number).unwrap_or(0.0);
        let info = values.get(3);
        let code = info
            .and_then(|i| i.get("code"))
            .and_then(Amf0Value::as_str)
            .unwrap_or("");
        let desc = info
            .and_then(|i| i.get("description"))
            .and_then(Amf0Value::as_str)
            .unwrap_or("");
        match name {
            "_result" if txid == TX_CONNECT && self.state == State::Connecting => {
                if !code.is_empty() && code != "NetConnection.Connect.Success" {
                    self.fail(events, code, desc);
                    return Ok(());
                }
                let key = Amf0Value::string(self.config.stream_key.clone());
                let enc = &self.link.encoder;
                let out = &mut self.link.out;
                for (cmd, tx) in [("releaseStream", TX_RELEASE), ("FCPublish", TX_FCPUBLISH)] {
                    write_command(
                        enc,
                        out,
                        CSID_COMMAND,
                        0,
                        &[
                            Amf0Value::string(cmd),
                            Amf0Value::Number(tx),
                            Amf0Value::Null,
                            key.clone(),
                        ],
                    );
                }
                write_command(
                    enc,
                    out,
                    CSID_COMMAND,
                    0,
                    &[
                        Amf0Value::string("createStream"),
                        Amf0Value::Number(TX_CREATE),
                        Amf0Value::Null,
                    ],
                );
                self.state = State::CreatingStream;
            }
            "_result" if txid == TX_CREATE && self.state == State::CreatingStream => {
                let id = values.get(3).and_then(Amf0Value::as_number).unwrap_or(1.0);
                self.stream_id = id as u32;
                write_command(
                    &self.link.encoder,
                    &mut self.link.out,
                    CSID_STREAM_COMMAND,
                    self.stream_id,
                    &[
                        Amf0Value::string("publish"),
                        Amf0Value::Number(TX_PUBLISH),
                        Amf0Value::Null,
                        Amf0Value::string(self.config.stream_key.clone()),
                        Amf0Value::string("live"),
                    ],
                );
                self.state = State::PublishPending;
            }
            "_error" if txid == TX_CONNECT || txid == TX_CREATE || txid == TX_PUBLISH => {
                let code = if code.is_empty() {
                    "NetConnection.Error"
                } else {
                    code
                };
                self.fail(events, code, desc);
            }
            "onStatus" => {
                let level = info
                    .and_then(|i| i.get("level"))
                    .and_then(Amf0Value::as_str);
                if code == "NetStream.Publish.Start" && self.state == State::PublishPending {
                    self.state = State::Publishing;
                    events.push(ClientEvent::Publishing);
                } else if level == Some("error") {
                    self.fail(events, code, desc);
                }
            }
            "close" => self.fail(
                events,
                "NetConnection.Close",
                "server closed the connection",
            ),
            _ => {}
        }
        Ok(())
    }
}
