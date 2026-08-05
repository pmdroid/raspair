//! RTSP request/response types and the small set of header and content-type
//! constants AirPlay uses.

use std::collections::BTreeMap;

pub const HEADER_CONTENT_TYPE: &str = "Content-Type";
pub const HEADER_CSEQ: &str = "CSeq";
pub const HEADER_SESSION: &str = "Session";
pub const HEADER_RTP_INFO: &str = "RTP-Info";
pub const HEADER_USER_AGENT: &str = "User-Agent";

pub const CONTENT_TYPE_BINARY_PLIST: &str = "application/x-apple-binary-plist";
pub const CONTENT_TYPE_OCTET_STREAM: &str = "application/octet-stream";
pub const CONTENT_TYPE_PARAMETERS: &str = "text/parameters";

// Receivers gate timing/session behavior on the claimed sender version; these
// mimic pyatv (proven audible on Apple TVs).
pub const USER_AGENT: &str = "AirPlay/550.10";
pub const USER_AGENT_PAIRING: &str = "AirPlay/320.20";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
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
