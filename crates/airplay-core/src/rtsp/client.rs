//! RTSP/1.0-over-TCP client. Carries the plaintext pairing exchanges, then
//! switches to the AirPlay 2 channel cipher for the encrypted control session.
//! Reads and writes share one connection; the channel cipher frames every byte
//! after [`Client::enable_encryption`].

use super::request::*;
use crate::pairing::{ChannelCipher, PairTransport};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub struct Client {
    addr: String,
    port: u16,
    stream: TcpStream,
    cipher: Option<ChannelCipher>,
    inbuf: Vec<u8>,
    cseq: i32,
    session: String,
    defaults: BTreeMap<String, String>,
    local_addr: Option<SocketAddr>,
}

impl Client {
    pub fn connect(addr: &str, port: u16) -> Result<Self> {
        let target = format!("{addr}:{port}");
        let sock: SocketAddr = target
            .to_socket_addrs_first()
            .with_context(|| format!("resolve {target}"))?;
        let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5))
            .with_context(|| format!("dial {target}"))?;
        Self::from_stream(addr, port, stream)
    }

    pub(crate) fn from_stream(addr: &str, port: u16, stream: TcpStream) -> Result<Self> {
        let local_addr = stream.local_addr().ok();
        Ok(Self {
            addr: addr.to_string(),
            port,
            stream,
            cipher: None,
            inbuf: Vec::new(),
            cseq: 0,
            session: String::new(),
            defaults: BTreeMap::new(),
            local_addr,
        })
    }

    pub fn local_ip(&self) -> Option<String> {
        self.local_addr.map(|a| a.ip().to_string())
    }

    pub fn set_default_header(&mut self, key: &str, value: &str) {
        self.defaults.insert(key.to_string(), value.to_string());
    }

    pub fn session_id(&self) -> &str {
        &self.session
    }

    /// Switch to the AirPlay 2 control-channel cipher. Must be called right
    /// after pair-verify (or transient pair-setup) with no plaintext buffered.
    pub fn enable_encryption(&mut self, shared_secret: &[u8]) -> Result<()> {
        if !self.inbuf.is_empty() {
            bail!("plaintext bytes still buffered at encryption switch");
        }
        self.cipher = Some(ChannelCipher::new_control(shared_secret));
        Ok(())
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        if let Some(cipher) = self.cipher.as_mut() {
            let enc = cipher.encrypt(data)?;
            self.stream.write_all(&enc)?;
        } else {
            self.stream.write_all(data)?;
        }
        Ok(())
    }

    fn fill(&mut self) -> Result<()> {
        if let Some(cipher) = self.cipher.as_mut() {
            let block = cipher.read_block(&mut self.stream)?;
            self.inbuf.extend_from_slice(&block);
        } else {
            let mut tmp = [0u8; 4096];
            let n = self.stream.read(&mut tmp)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed by peer").into());
            }
            self.inbuf.extend_from_slice(&tmp[..n]);
        }
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(idx) = self.inbuf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.inbuf[..idx]).to_string();
                self.inbuf.drain(..idx + 2);
                return Ok(line);
            }
            self.fill()?;
        }
    }

    fn read_exact_n(&mut self, n: usize) -> Result<Vec<u8>> {
        while self.inbuf.len() < n {
            self.fill()?;
        }
        let out = self.inbuf[..n].to_vec();
        self.inbuf.drain(..n);
        Ok(out)
    }

    fn read_response(&mut self) -> Result<Response> {
        let status_line = self.read_line().context("read status line")?;
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        let status_code = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        let mut headers = BTreeMap::new();
        loop {
            let line = self.read_line()?;
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_string(), v.trim().to_string());
            }
        }

        let mut body = Vec::new();
        let content_len = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(0);
        if content_len > 0 {
            body = self.read_exact_n(content_len).context("read body")?;
        }

        Ok(Response {
            status: status_line,
            status_code,
            headers,
            body,
        })
    }

    /// Send a plaintext HTTP/1.1 POST (the pairing exchanges that precede
    /// channel encryption).
    pub fn post_http(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Response> {
        self.cseq += 1;
        let mut buf = String::new();
        buf.push_str(&format!("POST {path} HTTP/1.1\r\n"));
        buf.push_str(&format!("Host: {}:{}\r\n", self.addr, self.port));
        buf.push_str(&format!("Content-Length: {}\r\n", body.len()));
        buf.push_str(&format!("Content-Type: {CONTENT_TYPE_OCTET_STREAM}\r\n"));
        buf.push_str(&format!("CSeq: {}\r\n", self.cseq));
        buf.push_str(&format!("User-Agent: {USER_AGENT_PAIRING}\r\n"));
        buf.push_str("Connection: keep-alive\r\n");
        for (k, v) in headers {
            buf.push_str(&format!("{k}: {v}\r\n"));
        }
        buf.push_str("\r\n");
        let mut out = buf.into_bytes();
        out.extend_from_slice(body);
        self.write_all(&out)?;
        self.read_response()
    }

    /// Send an RTSP request, applying default headers, CSeq, and the captured
    /// session id, and capturing any new session id from the response.
    pub fn do_request(&mut self, mut req: Request) -> Result<Response> {
        if !req.headers.contains_key(HEADER_CSEQ) {
            self.cseq += 1;
            req.set_header(HEADER_CSEQ, &self.cseq.to_string());
        }
        for (k, v) in &self.defaults {
            req.headers.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if !self.session.is_empty() {
            req.headers
                .entry(HEADER_SESSION.to_string())
                .or_insert_with(|| self.session.clone());
        }

        self.write_all(&req.serialize())?;
        let resp = self.read_response()?;

        if let Some(sess) = resp.header(HEADER_SESSION) {
            self.session = match sess.split_once(';') {
                Some((s, _)) => s.to_string(),
                None => sess.to_string(),
            };
        }
        Ok(resp)
    }

    pub fn get(&mut self, path: &str) -> Result<Response> {
        self.do_request(Request::new(Method::Get, path))
    }

    pub fn info(&mut self) -> Result<Response> {
        self.get("/info")
    }
}

impl PairTransport for Client {
    fn post(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>> {
        let resp = self.post_http(path, headers, body)?;
        if resp.status_code != 200 {
            return Err(crate::session::SessionError::ReceiverRejected { status: resp.status_code as u16 }.into());
        }
        Ok(resp.body)
    }
}

/// Minimal single-address resolver so we avoid pulling extra deps for the
/// common "ip:port" case while still supporting hostnames.
trait ToSocketAddrFirst {
    fn to_socket_addrs_first(&self) -> Result<SocketAddr>;
}

impl ToSocketAddrFirst for String {
    fn to_socket_addrs_first(&self) -> Result<SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("no address resolved for {self}"))
    }
}
