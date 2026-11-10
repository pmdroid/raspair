//! RTSP request/response types and the small set of header and content-type
//! constants AirPlay uses.

use md5::Digest;
use std::collections::BTreeMap;

pub const HEADER_CONTENT_TYPE: &str = "Content-Type";
pub const HEADER_CSEQ: &str = "CSeq";
pub const HEADER_SESSION: &str = "Session";
pub const HEADER_RTP_INFO: &str = "RTP-Info";
pub const HEADER_USER_AGENT: &str = "User-Agent";
pub const HEADER_AUTHORIZATION: &str = "Authorization";
pub const HEADER_WWW_AUTHENTICATE: &str = "WWW-Authenticate";

pub const CONTENT_TYPE_BINARY_PLIST: &str = "application/x-apple-binary-plist";
pub const CONTENT_TYPE_OCTET_STREAM: &str = "application/octet-stream";
pub const CONTENT_TYPE_PARAMETERS: &str = "text/parameters";
pub const CONTENT_TYPE_SDP: &str = "application/sdp";

// Receivers gate timing/session behavior on the claimed sender version; these
// mimic pyatv (proven audible on Apple TVs).
pub const USER_AGENT: &str = "AirPlay/550.10";
pub const USER_AGENT_PAIRING: &str = "AirPlay/320.20";
pub const DIGEST_USERNAME: &str = "iTunes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Options,
    Announce,
    Post,
    Setup,
    SetPeers,
    Record,
    Flush,
    FlushBuffered,
    SetRateAnchorTime,
    Teardown,
    SetParameter,
    GetParameter,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Options => "OPTIONS",
            Method::Announce => "ANNOUNCE",
            Method::Post => "POST",
            Method::Setup => "SETUP",
            Method::SetPeers => "SETPEERS",
            Method::Record => "RECORD",
            Method::Flush => "FLUSH",
            Method::FlushBuffered => "FLUSHBUFFERED",
            Method::SetRateAnchorTime => "SETRATEANCHORTIME",
            Method::Teardown => "TEARDOWN",
            Method::SetParameter => "SET_PARAMETER",
            Method::GetParameter => "GET_PARAMETER",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
}

impl Request {
    pub fn new(method: Method, url: &str) -> Self {
        Self {
            method,
            url: url.to_string(),
            headers: BTreeMap::new(),
            body: None,
        }
    }

    pub fn set_header(&mut self, key: &str, value: &str) {
        self.headers.insert(key.to_string(), value.to_string());
    }

    pub fn set_content_type(&mut self, ct: &str) {
        self.set_header(HEADER_CONTENT_TYPE, ct);
    }

    /// Attach a binary-plist body (sets Content-Type accordingly).
    pub fn set_body(&mut self, body: Vec<u8>) {
        self.body = Some(body);
        self.set_content_type(CONTENT_TYPE_BINARY_PLIST);
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = format!("{} {} RTSP/1.0\r\n", self.method.as_str(), self.url);
        if !self.headers.contains_key(HEADER_USER_AGENT) {
            buf.push_str(&format!("{HEADER_USER_AGENT}: {USER_AGENT}\r\n"));
        }
        for (k, v) in &self.headers {
            buf.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(body) = &self.body {
            buf.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        buf.push_str("\r\n");
        let mut out = buf.into_bytes();
        if let Some(body) = &self.body {
            out.extend_from_slice(body);
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: String,
    pub status_code: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, key: &str) -> Option<&str> {
        // RTSP header lookups are case-insensitive.
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("receiver requires a password")]
    PasswordRequired,
    #[error("receiver rejected the password")]
    Rejected,
    #[error("malformed authentication challenge")]
    MalformedChallenge,
    #[error("unsupported authentication challenge: {0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub stale: bool,
}

impl DigestChallenge {
    pub fn parse(header: &str) -> Result<Self, AuthError> {
        let header = header.trim();
        let (scheme, mut rest) = header.split_once(char::is_whitespace).unwrap_or((header, ""));
        if !scheme.eq_ignore_ascii_case("Digest") {
            return Err(AuthError::Unsupported(format!("scheme {scheme}")));
        }
        let mut params = BTreeMap::new();
        loop {
            rest = rest.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
            if rest.is_empty() {
                break;
            }
            let (key, tail) = rest.split_once('=').ok_or(AuthError::MalformedChallenge)?;
            let key = key.trim().to_ascii_lowercase();
            let (value, tail) = match tail.strip_prefix('"') {
                Some(quoted) => {
                    let end = quoted.find('"').ok_or(AuthError::MalformedChallenge)?;
                    (&quoted[..end], &quoted[end + 1..])
                }
                None => {
                    let (value, tail) = tail.split_at(tail.find(',').unwrap_or(tail.len()));
                    if value.trim().contains(char::is_whitespace) {
                        return Err(AuthError::MalformedChallenge);
                    }
                    (value, tail)
                }
            };
            let value = value.trim();
            if key.is_empty() || value.contains(['\\', '"']) || value.chars().any(char::is_control) {
                return Err(AuthError::MalformedChallenge);
            }
            if params.insert(key, value.to_string()).is_some() {
                return Err(AuthError::MalformedChallenge);
            }
            rest = tail;
        }
        if let Some(algorithm) = params.get("algorithm").filter(|a| !a.eq_ignore_ascii_case("MD5")) {
            return Err(AuthError::Unsupported(format!("algorithm {algorithm}")));
        }
        if params.contains_key("qop") {
            return Err(AuthError::Unsupported("qop".into()));
        }
        let field = |k: &str| params.get(k).filter(|v| !v.is_empty()).cloned().ok_or(AuthError::MalformedChallenge);
        Ok(Self {
            realm: field("realm")?,
            nonce: field("nonce")?,
            stale: params.get("stale").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        })
    }

    pub fn authorization(&self, method: Method, uri: &str, username: &str, password: &str) -> String {
        let md5 = |s: String| hex::encode(md5::Md5::digest(s.as_bytes()));
        let ha1 = md5(format!("{username}:{}:{password}", self.realm));
        let ha2 = md5(format!("{}:{uri}", method.as_str()));
        let response = md5(format!("{ha1}:{}:{ha2}", self.nonce));
        format!(
            "Digest username=\"{username}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
            self.realm, self.nonce
        )
    }
}
