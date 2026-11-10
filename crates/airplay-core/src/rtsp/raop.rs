use super::request::{Method, Request, Response, CONTENT_TYPE_SDP, HEADER_RTP_INFO};
use crate::client::AudioCodec;
use crate::protocol::{AuthRequirement, TxtRecord};
use crate::session::SessionError;
use std::net::IpAddr;

const CODEC_PCM: u8 = 0;
const CODEC_ALAC: u8 = 1;
const ENCRYPTION_NONE: u8 = 0;

pub fn codec(record: &TxtRecord) -> Result<AudioCodec, SessionError> {
    match &record.raop_codecs[..] {
        [] => Ok(AudioCodec::Alac),
        codecs if codecs.contains(&CODEC_ALAC) => Ok(AudioCodec::Alac),
        codecs if codecs.contains(&CODEC_PCM) => Ok(AudioCodec::Pcm),
        _ => Err(SessionError::UnsupportedProfile),
    }
}

pub fn admit(record: &TxtRecord) -> Result<AudioCodec, SessionError> {
    match record.auth_requirement() {
        AuthRequirement::None | AuthRequirement::Password | AuthRequirement::LegacyPin => {}
        other => return Err(SessionError::UnsupportedAuthentication(other)),
    }
    if !record.raop_encryption.contains(&ENCRYPTION_NONE) {
        return Err(SessionError::UnsupportedProfile);
    }
    codec(record)
}

fn family(ip: &str) -> &'static str {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => "IP6",
        _ => "IP4",
    }
}

pub fn sdp(session: u32, local_ip: &str, remote_ip: &str, codec: AudioCodec, sample_rate: i32, spf: i32) -> String {
    let format = match codec {
        AudioCodec::Pcm => format!("a=rtpmap:96 L16/{sample_rate}/2\r\n"),
        _ => format!("a=rtpmap:96 AppleLossless\r\na=fmtp:96 {spf} 0 16 40 10 14 2 255 0 0 {sample_rate}\r\n"),
    };
    format!(
        "v=0\r\no=iTunes {session} 0 IN {} {local_ip}\r\ns=iTunes\r\nc=IN {} {remote_ip}\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n{format}",
        family(local_ip),
        family(remote_ip)
    )
}

pub fn new_announce(url: &str, sdp: &str) -> Request {
    let mut req = Request::new(Method::Announce, url);
    req.body = Some(sdp.as_bytes().to_vec());
    req.set_content_type(CONTENT_TYPE_SDP);
    req
}

pub fn new_setup(url: &str, control_port: u16, timing_port: u16) -> Request {
    let mut req = Request::new(Method::Setup, url);
    let transport = format!("RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={control_port};timing_port={timing_port}");
    req.set_header("Transport", &transport);
    req
}

pub fn parse_transport(resp: &Response) -> Result<(i64, i64), SessionError> {
    let transport = resp.header("Transport").ok_or(SessionError::ReceiverRejected { status: resp.status_code })?;
    let port = |key: &str| {
        transport
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| v.parse::<u16>().ok())
            .filter(|p| *p != 0)
    };
    match (port("server_port"), port("control_port")) {
        (Some(data), Some(control)) => Ok((data as i64, control as i64)),
        _ => Err(SessionError::ReceiverRejected { status: resp.status_code }),
    }
}

pub fn new_record(url: &str, seq: u16, rtptime: u32) -> Request {
    let mut req = Request::new(Method::Record, url);
    req.set_header("Range", "npt=0-");
    req.set_header(HEADER_RTP_INFO, &format!("seq={seq};rtptime={rtptime}"));
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_txt_record;
    use std::collections::BTreeMap;

    fn record(txt: &[&str]) -> TxtRecord {
        parse_txt_record(&txt.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn admission_follows_advertised_codecs_encryption_and_auth() {
        let base = ["am=AirPort4,107", "sf=0x4"];
        let with = |extra: &[&str]| record(&[&base[..], extra].concat());
        assert_eq!(admit(&with(&["cn=0,1", "et=0,1"])), Ok(AudioCodec::Alac));
        assert_eq!(admit(&with(&["cn=0", "et=0,4"])), Ok(AudioCodec::Pcm));
        assert_eq!(admit(&with(&["et=0"])), Ok(AudioCodec::Alac));
        assert_eq!(admit(&with(&["cn=2,3", "et=0"])), Err(SessionError::UnsupportedProfile));
        assert_eq!(admit(&with(&["cn=0,1", "et=1"])), Err(SessionError::UnsupportedProfile));
        assert_eq!(admit(&with(&["cn=0,1"])), Err(SessionError::UnsupportedProfile));
        assert_eq!(admit(&record(&["am=AirPort4,107", "sf=0xc", "et=0"])), Ok(AudioCodec::Alac));
        assert_eq!(admit(&record(&["am=AirPort4,107", "sf=0x4", "pw=true", "et=0"])), Ok(AudioCodec::Alac));
        let unknown = record(&["et=0"]);
        assert!(matches!(admit(&unknown), Err(SessionError::UnsupportedAuthentication(_))));
    }

    #[test]
    fn transport_requires_negotiated_data_and_control_ports() {
        let resp = |transport: Option<&str>| Response {
            status: "RTSP/1.0 200 OK".into(),
            status_code: 200,
            headers: transport.map(|t| ("transport".to_string(), t.to_string())).into_iter().collect::<BTreeMap<_, _>>(),
            body: Vec::new(),
        };
        let ok = resp(Some("RTP/AVP/UDP;unicast;mode=record;server_port=6000;control_port=6001;timing_port=6002"));
        assert_eq!(parse_transport(&ok), Ok((6000, 6001)));
        for bad in [None, Some("RTP/AVP/UDP;server_port=6000"), Some("RTP/AVP/UDP;server_port=0;control_port=6001")] {
            assert_eq!(parse_transport(&resp(bad)), Err(SessionError::ReceiverRejected { status: 200 }));
        }
    }
}
