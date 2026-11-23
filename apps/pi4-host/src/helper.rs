use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub const SOCKET: &str = "/run/vinyl-bridge-helper/helper.sock";
const MAX_REQUEST: u64 = 4096;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Profile {
    DacPlusAdc,
    DacPlusAdcPro,
    Dac2AdcPro,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub ssid: String,
    pub passphrase: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ApplyProfile { profile: Profile },
    ApplyNetwork { network: Network },
    Reboot {},
}

impl Network {
    fn validate(&self) -> Result<()> {
        if self.ssid.is_empty() || self.ssid.len() > 32 || self.ssid.chars().any(char::is_control) {
            bail!("ssid must be 1-32 bytes without control characters");
        }
        let p = &self.passphrase;
        let ascii = (8..=63).contains(&p.len()) && p.bytes().all(|b| (0x20..0x7f).contains(&b));
        let hex = p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit());
        if !ascii && !hex {
            bail!("passphrase must be 8-63 printable ASCII characters or 64 hex digits");
        }
        Ok(())
    }
}

pub trait Ops {
    fn apply_profile(&mut self, profile: Profile) -> Result<()>;
    fn apply_network(&mut self, network: &Network) -> Result<()>;
    fn reboot(&mut self) -> Result<()>;
}

pub struct HostOps;

impl Ops for HostOps {
    fn apply_profile(&mut self, profile: Profile) -> Result<()> {
        bail!("applying {profile:?} is not supported yet")
    }

    fn apply_network(&mut self, _network: &Network) -> Result<()> {
        bail!("network changes are not supported yet")
    }

    fn reboot(&mut self) -> Result<()> {
        let status = std::process::Command::new("systemctl").arg("reboot").status().context("run systemctl reboot")?;
        if !status.success() {
            bail!("systemctl reboot failed: {status}");
        }
        Ok(())
    }
}

pub fn dispatch(line: &[u8], ops: &mut dyn Ops) -> Result<()> {
    let request: Request = serde_json::from_slice(line).context("invalid request")?;
    match request {
        Request::ApplyProfile { profile } => ops.apply_profile(profile),
        Request::ApplyNetwork { network } => {
            network.validate()?;
            ops.apply_network(&network)
        }
        Request::Reboot {} => ops.reboot(),
    }
}

async fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut reader = BufReader::new(stream.take(MAX_REQUEST + 1));
    tokio::time::timeout(READ_TIMEOUT, reader.read_until(b'\n', &mut line)).await.context("request timed out")??;
    if line.len() as u64 > MAX_REQUEST {
        bail!("request exceeds {MAX_REQUEST} bytes");
    }
    Ok(line)
}

pub async fn serve(mut stream: UnixStream, allowed_uid: u32, ops: &mut dyn Ops) -> Result<()> {
    let result = match stream.peer_cred() {
        Ok(cred) if cred.uid() == allowed_uid => match read_line(&mut stream).await {
            Ok(line) => dispatch(&line, ops),
            Err(e) => Err(e),
        },
        Ok(cred) => Err(anyhow::anyhow!("peer uid {} is not allowed", cred.uid())),
        Err(e) => Err(anyhow::Error::from(e).context("read peer credentials")),
    };
    let reply = match &result {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
    };
    let _ = stream.write_all(format!("{reply}\n").as_bytes()).await;
    result
}

pub async fn run() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (uid, gid) = crate::install::service_ids()?;
    let path = std::path::Path::new(SOCKET);
    if path.symlink_metadata().is_ok() {
        std::fs::remove_file(path).with_context(|| format!("remove stale {SOCKET}"))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {SOCKET}"))?;
    std::os::unix::fs::lchown(path, Some(0), Some(gid))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    tracing::info!(socket = SOCKET, "system helper listening");
    let mut ops = HostOps;
    loop {
        let (stream, _) = listener.accept().await?;
        if let Err(e) = serve(stream, uid, &mut ops).await {
            tracing::warn!("helper request rejected: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeOps {
        calls: Vec<String>,
    }

    impl Ops for FakeOps {
        fn apply_profile(&mut self, profile: Profile) -> Result<()> {
            self.calls.push(format!("profile {profile:?}"));
            Ok(())
        }

        fn apply_network(&mut self, network: &Network) -> Result<()> {
            self.calls.push(format!("network {}", network.ssid));
            Ok(())
        }

        fn reboot(&mut self) -> Result<()> {
            self.calls.push("reboot".into());
            Ok(())
        }
    }

    async fn exchange(request: &[u8], allowed_uid: u32, ops: &mut FakeOps) -> (Result<()>, String) {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).await.unwrap();
        client.shutdown().await.unwrap();
        let result = serve(server, allowed_uid, ops).await;
        let mut reply = String::new();
        let _ = client.read_to_string(&mut reply).await;
        (result, reply)
    }

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    #[tokio::test]
    async fn valid_reboot_is_dispatched() {
        let mut ops = FakeOps::default();
        let (result, reply) = exchange(b"{\"op\":\"reboot\"}\n", me(), &mut ops).await;
        result.unwrap();
        assert_eq!(reply, "{\"ok\":true}\n");
        assert_eq!(ops.calls, ["reboot"]);
    }

    #[tokio::test]
    async fn wrong_peer_uid_is_rejected_without_ops() {
        let mut ops = FakeOps::default();
        let (result, _) = exchange(b"{\"op\":\"reboot\"}\n", me().wrapping_add(1), &mut ops).await;
        assert!(result.unwrap_err().to_string().contains("is not allowed"));
        assert!(ops.calls.is_empty());
    }

    #[tokio::test]
    async fn invalid_unknown_and_oversized_requests_are_rejected() {
        let long_ssid = format!("{{\"op\":\"apply_network\",\"network\":{{\"ssid\":\"{}\",\"passphrase\":\"password1\"}}}}\n", "x".repeat(33));
        let oversized = format!("{{\"op\":\"reboot\",\"pad\":\"{}\"}}\n", "x".repeat(MAX_REQUEST as usize));
        let cases: Vec<&[u8]> = vec![
            b"not json\n",
            b"{\"op\":\"shell\",\"cmd\":\"rm -rf /\"}\n",
            b"{\"op\":\"reboot\",\"unit\":\"ssh.service\"}\n",
            b"{\"op\":\"apply_profile\",\"profile\":\"DacPlusAdc\",\"path\":\"/boot/firmware/config.txt\"}\n",
            b"{\"op\":\"apply_profile\",\"profile\":\"../../etc/shadow\"}\n",
            b"{\"op\":\"apply_network\",\"network\":{\"ssid\":\"home\",\"passphrase\":\"short\"}}\n",
            b"{\"op\":\"apply_network\",\"network\":{\"ssid\":\"home\",\"passphrase\":\"password1\",\"interface\":\"eth0\"}}\n",
            long_ssid.as_bytes(),
            oversized.as_bytes(),
        ];
        for request in cases {
            let mut ops = FakeOps::default();
            let (result, reply) = exchange(request, me(), &mut ops).await;
            assert!(result.is_err(), "{}", String::from_utf8_lossy(request));
            assert!(reply.is_empty() || reply.contains("\"ok\":false"), "{reply}");
            assert!(ops.calls.is_empty());
        }
    }

    #[tokio::test]
    async fn named_profile_and_valid_network_reach_ops() {
        let mut ops = FakeOps::default();
        for request in [&b"{\"op\":\"apply_profile\",\"profile\":\"Dac2AdcPro\"}\n"[..], b"{\"op\":\"apply_network\",\"network\":{\"ssid\":\"home\",\"passphrase\":\"password1\"}}"] {
            exchange(request, me(), &mut ops).await.0.unwrap();
        }
        assert_eq!(ops.calls, ["profile Dac2AdcPro", "network home"]);
    }
}
