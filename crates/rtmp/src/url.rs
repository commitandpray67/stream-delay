//! Parsing of `rtmp://` and `rtmps://` destination URLs.

use std::fmt;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Rtmp,
    Rtmps,
}

/// A parsed destination such as `rtmp://live.twitch.tv/app` or
/// `rtmps://a.rtmps.youtube.com:443/live2/<key>`.
#[derive(Clone, PartialEq, Eq)]
pub struct RtmpUrl {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    /// The RTMP application name (first path segment, plus a query string if present).
    pub app: String,
    /// `scheme://host[:port]/app`, sent as `tcUrl` in `connect`.
    pub tc_url: String,
    /// Remaining path segments, if the URL embeds the stream key.
    pub stream_key: Option<String>,
}

impl fmt::Debug for RtmpUrl {
    // Never print the stream key, or the query (see [`RtmpUrl::redacted`]).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtmpUrl")
            .field("tc_url", &self.redacted())
            .field(
                "stream_key",
                &self.stream_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UrlError {
    #[error("URL must start with rtmp:// or rtmps://")]
    Scheme,
    #[error("URL has no host")]
    Host,
    #[error("invalid port")]
    Port,
    #[error("URL has no application path (for example rtmp://host/app)")]
    App,
    #[error("a user name or password in the URL is not supported")]
    Credentials,
}

impl RtmpUrl {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        let input = input.trim();
        let (scheme, rest) = if let Some(r) = strip_prefix_ci(input, "rtmp://") {
            (Scheme::Rtmp, r)
        } else if let Some(r) = strip_prefix_ci(input, "rtmps://") {
            (Scheme::Rtmps, r)
        } else {
            return Err(UrlError::Scheme);
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.contains('@') {
            return Err(UrlError::Credentials);
        }
        if authority.contains(['?', '#']) {
            return Err(UrlError::Host);
        }
        let default_port = match scheme {
            Scheme::Rtmp => 1935,
            Scheme::Rtmps => 443,
        };
        let (host, port) = split_host_port(authority, default_port)?;
        if host.is_empty() {
            return Err(UrlError::Host);
        }
        let path = path.trim_matches('/');
        let (app, key) = match path.split_once('/') {
            Some((a, k)) => (a, Some(k)),
            None => (path, None),
        };
        if app.is_empty() {
            return Err(UrlError::App);
        }
        let scheme_str = match scheme {
            Scheme::Rtmp => "rtmp",
            Scheme::Rtmps => "rtmps",
        };
        let authority_str = if port == default_port {
            host_for_url(&host)
        } else {
            format!("{}:{port}", host_for_url(&host))
        };
        Ok(Self {
            scheme,
            tc_url: format!("{scheme_str}://{authority_str}/{app}"),
            host,
            port,
            app: app.to_string(),
            stream_key: key.filter(|k| !k.is_empty()).map(str::to_string),
        })
    }

    /// The URL without any embedded stream key, or the values of its query,
    /// where some servers take a password: safe to show and log.
    pub fn redacted(&self) -> String {
        match self.tc_url.split_once('?') {
            Some((base, _)) => format!("{base}?…"),
            None => self.tc_url.clone(),
        }
    }

    /// The query of the application (`rtmp://host/app?<query>`), which may hold
    /// credentials. Without the `?`.
    pub fn query(&self) -> Option<&str> {
        self.app
            .split_once('?')
            .map(|(_, q)| q)
            .filter(|q| !q.is_empty())
    }

    /// The values in [`RtmpUrl::query`], and the query itself: what must not be
    /// shown or logged.
    pub fn query_secrets(&self) -> Vec<String> {
        let Some(q) = self.query() else {
            return Vec::new();
        };
        let mut v = vec![q.to_string()];
        v.extend(
            q.split('&')
                .map(|pair| pair.split_once('=').map_or(pair, |(_, value)| value))
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        );
        v
    }
}

fn host_for_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `get` rather than indexing: the input may have a multi-byte character where
    // the prefix would end.
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(String, u16), UrlError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(UrlError::Host)?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| UrlError::Port)?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().map_err(|_| UrlError::Port)?)),
        None => Ok((authority.to_string(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twitch() {
        let u = RtmpUrl::parse("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!(u.scheme, Scheme::Rtmp);
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str()),
            ("live.twitch.tv", 1935, "app")
        );
        assert_eq!(u.tc_url, "rtmp://live.twitch.tv/app");
        assert_eq!(u.stream_key, None);
    }

    #[test]
    fn rtmps_with_port_and_key() {
        let u = RtmpUrl::parse("RTMPS://a.rtmps.youtube.com:443/live2/abcd-efgh").unwrap();
        assert_eq!(u.scheme, Scheme::Rtmps);
        assert_eq!(u.port, 443);
        assert_eq!(u.tc_url, "rtmps://a.rtmps.youtube.com/live2");
        assert_eq!(u.stream_key.as_deref(), Some("abcd-efgh"));
        assert!(!format!("{u:?}").contains("abcd"));
    }

    #[test]
    fn custom_port_and_ipv6() {
        let u = RtmpUrl::parse("rtmp://127.0.0.1:1936/live/").unwrap();
        assert_eq!(u.port, 1936);
        assert_eq!(u.tc_url, "rtmp://127.0.0.1:1936/live");
        let u = RtmpUrl::parse("rtmp://[::1]:1940/live").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 1940));
        assert_eq!(u.tc_url, "rtmp://[::1]:1940/live");
    }

    #[test]
    fn query_values_are_never_shown() {
        let u =
            RtmpUrl::parse("rtmps://ingest.example/live?auth=SENTINEL_credential_123&x=1").unwrap();
        assert_eq!(u.app, "live?auth=SENTINEL_credential_123&x=1");
        assert_eq!(u.query(), Some("auth=SENTINEL_credential_123&x=1"));
        assert_eq!(u.redacted(), "rtmps://ingest.example/live?…");
        assert!(!format!("{u:?}").contains("SENTINEL"));
        assert_eq!(
            u.query_secrets(),
            [
                "auth=SENTINEL_credential_123&x=1",
                "SENTINEL_credential_123",
                "1"
            ]
        );
        let u = RtmpUrl::parse("rtmp://host/app").unwrap();
        assert_eq!(
            (u.query(), u.redacted().as_str()),
            (None, "rtmp://host/app")
        );
        assert!(u.query_secrets().is_empty());
    }

    #[test]
    fn errors() {
        assert_eq!(
            RtmpUrl::parse("rtmp://user:secret@host:1935/app"),
            Err(UrlError::Credentials)
        );
        assert_eq!(RtmpUrl::parse("rtmp://host?x=1/app"), Err(UrlError::Host));
        assert_eq!(RtmpUrl::parse("http://x/app"), Err(UrlError::Scheme));
        assert_eq!(RtmpUrl::parse("rtmp://host"), Err(UrlError::App));
        assert_eq!(RtmpUrl::parse("rtmp://host:abc/app"), Err(UrlError::Port));
        // A multi-byte character where the scheme would end.
        assert_eq!(RtmpUrl::parse("aaaaaaé"), Err(UrlError::Scheme));
        assert_eq!(RtmpUrl::parse("rtmpé://x/app"), Err(UrlError::Scheme));
    }

    proptest::proptest! {
        /// Settings and the command line pass arbitrary text here.
        #[test]
        fn arbitrary_text_never_panics(s in ".*", tail in ".*") {
            let _ = RtmpUrl::parse(&s);
            let _ = RtmpUrl::parse(&format!("rtmp://{tail}"));
            let _ = RtmpUrl::parse(&format!("rtmps://[{tail}"));
        }
    }
}
