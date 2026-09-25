//! Connection setup: ProtocolVersion, security negotiation, ClientInit / ServerInit.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use super::wire::{self, MAX_NAME};
use crate::auth::vnc_auth_response;
use crate::{Config, Error};

/// Longest failure reason we read from the server.
const MAX_REASON: u32 = 64 * 1024;
/// Reason reported when the server gives none.
const GENERIC_FAILURE: &str = "authentication failed";

/// Protocol version agreed with the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Version {
    V3_3,
    V3_7,
    V3_8,
}

impl Version {
    /// Map the server's ProtocolVersion message to the version we speak.
    fn parse(raw: &[u8; 12]) -> Result<Self, Error> {
        let unsupported = || Error::Version(String::from_utf8_lossy(raw).trim_end().to_owned());
        if &raw[..4] != b"RFB " || raw[7] != b'.' || raw[11] != b'\n' {
            return Err(unsupported());
        }
        let (Some(major), Some(minor)) = (parse_digits(&raw[4..7]), parse_digits(&raw[8..11])) else {
            return Err(unsupported());
        };
        match (major, minor) {
            (3, 3) => Ok(Self::V3_3),
            (3, 7) => Ok(Self::V3_7),
            // 3.8 and anything newer (e.g. Apple's 3.889) speak the 3.8 handshake.
            v if v >= (3, 8) => Ok(Self::V3_8),
            _ => Err(unsupported()),
        }
    }

    fn reply(self) -> &'static [u8; 12] {
        match self {
            Self::V3_3 => b"RFB 003.003\n",
            Self::V3_7 => b"RFB 003.007\n",
            Self::V3_8 => b"RFB 003.008\n",
        }
    }
}

fn parse_digits(digits: &[u8]) -> Option<u16> {
    digits.iter().try_fold(0u16, |acc, &d| d.is_ascii_digit().then(|| acc * 10 + u16::from(d - b'0')))
}

/// Security type chosen for the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Security {
    None,
    VncAuth,
}

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;

/// Pick "None" if offered, else VNC Authentication (which needs a password).
fn choose_security(offered: &[u8], config: &Config) -> Result<Security, Error> {
    if offered.contains(&SECURITY_NONE) {
        Ok(Security::None)
    } else if offered.contains(&SECURITY_VNC_AUTH) {
        match config.password {
            Some(_) => Ok(Security::VncAuth),
            None => Err(Error::PasswordRequired),
        }
    } else {
        Err(Error::NoSecurity(offered.to_vec()))
    }
}

/// What the server told us in ServerInit (size already validated).
#[derive(Debug)]
pub(super) struct ServerInit {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) name: String,
}

/// Run the handshake up to and including ServerInit.
pub(super) async fn handshake<R, W>(reader: &mut R, writer: &mut W, config: &Config) -> Result<ServerInit, Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let version = exchange_version(reader, writer).await?;
    let security = negotiate_security(reader, writer, version, config).await?;
    authenticate(reader, writer, version, security, config).await?;
    wire::send(writer, &[u8::from(config.shared)]).await?;
    read_server_init(reader).await
}

async fn exchange_version<R, W>(reader: &mut R, writer: &mut W) -> Result<Version, Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut raw = [0u8; 12];
    reader.read_exact(&mut raw).await?;
    let version = Version::parse(&raw)?;
    tracing::debug!(server = %String::from_utf8_lossy(&raw).trim_end(), ?version, "RFB version");
    wire::send(writer, version.reply()).await?;
    Ok(version)
}

async fn negotiate_security<R, W>(
    reader: &mut R,
    writer: &mut W,
    version: Version,
    config: &Config,
) -> Result<Security, Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if version == Version::V3_3 {
        // 3.3: the server decides and sends a u32 type.
        return match reader.read_u32().await? {
            0 => Err(Error::AuthFailed(read_reason(reader).await)),
            1 => Ok(Security::None),
            2 => choose_security(&[SECURITY_VNC_AUTH], config),
            other => Err(Error::NoSecurity(vec![u8::try_from(other).unwrap_or(u8::MAX)])),
        };
    }
    let count = reader.read_u8().await?;
    if count == 0 {
        return Err(Error::AuthFailed(read_reason(reader).await));
    }
    let mut offered = vec![0u8; usize::from(count)];
    reader.read_exact(&mut offered).await?;
    let security = choose_security(&offered, config)?;
    tracing::debug!(?offered, ?security, "security negotiated");
    let chosen = match security {
        Security::None => SECURITY_NONE,
        Security::VncAuth => SECURITY_VNC_AUTH,
    };
    wire::send(writer, &[chosen]).await?;
    Ok(security)
}

async fn authenticate<R, W>(
    reader: &mut R,
    writer: &mut W,
    version: Version,
    security: Security,
    config: &Config,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match security {
        Security::VncAuth => {
            let password = config.password.as_deref().ok_or(Error::PasswordRequired)?;
            let mut challenge = [0u8; 16];
            reader.read_exact(&mut challenge).await?;
            wire::send(writer, &vnc_auth_response(password, &challenge)).await?;
            security_result(reader, version).await
        }
        // Before 3.8 there is no SecurityResult after "None".
        Security::None if version >= Version::V3_8 => security_result(reader, version).await,
        Security::None => Ok(()),
    }
}

async fn security_result<R: AsyncRead + Unpin>(reader: &mut R, version: Version) -> Result<(), Error> {
    if reader.read_u32().await? == 0 {
        return Ok(());
    }
    let reason = if version >= Version::V3_8 { read_reason(reader).await } else { GENERIC_FAILURE.to_owned() };
    tracing::debug!(%reason, "security handshake failed");
    Err(Error::AuthFailed(reason))
}

/// Failure reason string (trimmed, without the C terminator QEMU sends); an empty reason or any
/// problem reading it falls back to a generic text so the caller still reports an
/// authentication failure.
async fn read_reason<R: AsyncRead + Unpin>(reader: &mut R) -> String {
    let reason = wire::read_string(reader, MAX_REASON, "failure reason").await.unwrap_or_default();
    match reason.trim() {
        "" => GENERIC_FAILURE.to_owned(),
        reason => reason.to_owned(),
    }
}

async fn read_server_init<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ServerInit, Error> {
    let mut head = [0u8; 20]; // width, height, pixel format (ignored: we set our own)
    reader.read_exact(&mut head).await?;
    let width = u16::from_be_bytes([head[0], head[1]]);
    let height = u16::from_be_bytes([head[2], head[3]]);
    let name = wire::read_string(reader, MAX_NAME, "desktop name").await?;
    let (width, height) = wire::check_size(width, height)?;
    tracing::debug!(width, height, %name, "ServerInit");
    Ok(ServerInit { width, height, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Version, Error> {
        Version::parse(s.as_bytes().try_into().unwrap())
    }

    #[test]
    fn versions() {
        assert_eq!(parse("RFB 003.003\n").unwrap(), Version::V3_3);
        assert_eq!(parse("RFB 003.007\n").unwrap(), Version::V3_7);
        assert_eq!(parse("RFB 003.008\n").unwrap(), Version::V3_8);
        assert_eq!(parse("RFB 003.889\n").unwrap(), Version::V3_8);
        assert_eq!(parse("RFB 004.001\n").unwrap(), Version::V3_8);
        for bad in ["RFB 003.005\n", "RFB 002.009\n", "RFB 00a.008\n", "HTTP/1.1 400", "RFB 003,008\n"] {
            assert!(matches!(parse(bad), Err(Error::Version(_))), "{bad:?}");
        }
    }

    #[test]
    fn security_preference() {
        let none = Config::default();
        let pw = Config::with_password("x");
        assert_eq!(choose_security(&[2, 1], &none).unwrap(), Security::None);
        assert_eq!(choose_security(&[19, 2], &pw).unwrap(), Security::VncAuth);
        assert!(matches!(choose_security(&[2], &none), Err(Error::PasswordRequired)));
        assert!(matches!(choose_security(&[19, 16], &pw), Err(Error::NoSecurity(v)) if v == [19, 16]));
    }
}
