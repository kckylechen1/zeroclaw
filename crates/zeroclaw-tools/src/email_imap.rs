/// Shared IMAP connection utility used by email_search and email_read.
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use async_imap::Session;
use async_imap::error::{Error as ImapError, Result as ImapResult};
use async_imap::imap_proto::{
    AttributeValue, MailboxDatum, MessageSection, Response, SectionPath, Status,
};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::DnsName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use zeroclaw_config::scattered_types::EmailConfig;

/// Tolerates TLS streams from servers that omit `close_notify` (e.g. Exchange).
pub struct TlsStreamTolerant(pub TlsStream<TcpStream>);

impl AsyncRead for TlsStreamTolerant {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            Poll::Ready(Err(e)) if is_tls_close_notify(&e) => Poll::Ready(Ok(())),
            other => other,
        }
    }
}

impl AsyncWrite for TlsStreamTolerant {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl fmt::Debug for TlsStreamTolerant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TlsStreamTolerant")
    }
}

/// Detects the missing-`close_notify` truncation that Exchange-hosted IMAP
/// servers produce. `ErrorKind::UnexpectedEof` is the stable signal; the
/// string match is a fallback for rustls variants that only surface it in the
/// display text.
pub fn is_tls_close_notify(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::UnexpectedEof || e.to_string().contains("close_notify")
}

pub type ImapSession = Session<TlsStreamTolerant>;

// chatmail/async-imap#95: FETCH/SEARCH helpers discard tagged NO/BAD completions.
// Keep its parser, but publish collected data only after this command's OK.
async fn collect_command<T, R>(
    session: &mut Session<T>,
    command: &str,
    mut collect: impl FnMut(&mut R, &Response<'_>) + Send,
) -> ImapResult<R>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
    R: Default + Send,
{
    let request_id = session.run_command(command).await?;
    let mut result = R::default();
    loop {
        let response = session
            .read_response()
            .await?
            .ok_or(ImapError::ConnectionLost)?;
        match response.parsed() {
            Response::Done {
                tag,
                status,
                outcome,
            } if tag == &request_id => {
                return match status {
                    Status::Ok => Ok(result),
                    Status::No => Err(ImapError::No(format!("outcome: {outcome:?}"))),
                    Status::Bad => Err(ImapError::Bad(format!("outcome: {outcome:?}"))),
                    _ => Err(ImapError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected IMAP completion status: {status:?}"),
                    ))),
                };
            }
            Response::Done { .. } => {}
            Response::Data {
                status: Status::Bye,
                ..
            } => return Err(ImapError::ConnectionLost),
            // These tools use one-shot read-only sessions, not mailbox sync.
            // Unrelated mailbox updates must not become command results.
            response => collect(&mut result, response),
        }
    }
}

pub(crate) async fn uid_search<T>(
    session: &mut Session<T>,
    criteria: &str,
) -> ImapResult<HashSet<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    collect_command(
        session,
        &format!("UID SEARCH {criteria}"),
        |ids: &mut HashSet<u32>, response| {
            if let Response::MailboxData(MailboxDatum::Search(found)) = response {
                ids.extend(found);
            }
        },
    )
    .await
}

#[derive(Clone, Copy)]
pub(crate) enum PeekPart {
    Header,
    Body,
}

#[derive(Debug)]
pub(crate) struct PeekMessage {
    pub(crate) uid: u32,
    pub(crate) data: Option<Vec<u8>>,
}

pub(crate) async fn uid_fetch_peek<T>(
    session: &mut Session<T>,
    uids: &[u32],
    part: PeekPart,
) -> ImapResult<Vec<PeekMessage>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    // A zero search limit selects no UIDs; IMAP has no empty sequence set.
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let query = match part {
        PeekPart::Header => "BODY.PEEK[HEADER]",
        PeekPart::Body => "BODY.PEEK[]",
    };
    collect_command(
        session,
        &format!("UID FETCH {uid_set} {query}"),
        |messages: &mut Vec<PeekMessage>, response| {
            let Response::Fetch(_, attributes) = response else {
                return;
            };
            let Some(uid) = attributes.iter().find_map(|attribute| match attribute {
                AttributeValue::Uid(uid) if uids.contains(uid) => Some(*uid),
                _ => None,
            }) else {
                return;
            };
            // A unilateral FETCH may contain only flags, or concern another UID.
            // Only the requested message part belongs in this command's result.
            let data = attributes
                .iter()
                .find_map(|attribute| match (part, attribute) {
                    (
                        PeekPart::Header,
                        AttributeValue::BodySection {
                            section: Some(SectionPath::Full(MessageSection::Header)),
                            data,
                            ..
                        }
                        | AttributeValue::Rfc822Header(data),
                    )
                    | (
                        PeekPart::Body,
                        AttributeValue::BodySection {
                            section: None,
                            data,
                            ..
                        }
                        | AttributeValue::Rfc822(data),
                    ) => Some(data.as_deref().map(<[u8]>::to_vec)),
                    _ => None,
                });
            if let Some(data) = data {
                messages.push(PeekMessage { uid, data });
            }
        },
    )
    .await
}

pub async fn imap_connect(
    cfg: &EmailConfig,
    auth_service: Option<&Arc<zeroclaw_providers::auth::AuthService>>,
    alias: &str,
) -> Result<ImapSession> {
    let addr = format!("{}:{}", cfg.imap_host, cfg.imap_port);
    let tcp = TcpStream::connect(&addr)
        .await
        .context("TCP connect failed")?;

    let certs = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let tls_config = ClientConfig::builder()
        .with_root_certificates(certs)
        .with_no_client_auth();
    let connector: TlsConnector = Arc::new(tls_config).into();
    let sni: DnsName = cfg
        .imap_host
        .clone()
        .try_into()
        .context("invalid hostname")?;
    let raw = connector
        .connect(sni.into(), tcp)
        .await
        .context("TLS handshake failed")?;
    let stream = TlsStreamTolerant(raw);

    let mut client = async_imap::Client::new(stream);
    client.read_response().await.context("no IMAP greeting")?;

    if let Some(oauth2_cfg) = &cfg.oauth2 {
        let token = if let Some(svc) = auth_service {
            let channel_key = format!("email.{}", alias);
            svc.get_valid_email_oauth2_token(
                &channel_key,
                None,
                &oauth2_cfg.token_url,
                &oauth2_cfg.client_id,
                &oauth2_cfg.scopes,
            )
            .await?
            .ok_or_else(|| anyhow::Error::msg(format!("no OAuth2 token available for {}", alias)))?
        } else {
            anyhow::bail!(
                "oauth2 configured for '{}' but no auth service provided",
                alias
            );
        };

        struct XOAuth2 {
            user: String,
            token: String,
        }
        impl async_imap::Authenticator for XOAuth2 {
            type Response = String;
            fn process(&mut self, _: &[u8]) -> String {
                format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
            }
        }
        client
            .authenticate(
                "XOAUTH2",
                XOAuth2 {
                    user: cfg.username.clone(),
                    token,
                },
            )
            .await
            .map_err(|(e, _)| anyhow::Error::msg(format!("XOAUTH2 auth failed: {}", e)))
    } else {
        client
            .login(&cfg.username, &cfg.password)
            .await
            .map_err(|(e, _)| anyhow::Error::msg(format!("IMAP login failed: {}", e)))
    }
}

#[cfg(test)]
#[path = "email_imap/protocol_tests.rs"]
mod protocol_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_tls_close_notify_matches_unexpected_eof() {
        // Exchange-style truncation lands here. The ErrorKind is the stable
        // signal; message text is irrelevant for this branch.
        let e = io::Error::new(io::ErrorKind::UnexpectedEof, "anything");
        assert!(is_tls_close_notify(&e));
    }

    #[test]
    fn is_tls_close_notify_matches_string_marker() {
        // Some rustls variants only surface close_notify via the Display
        // string. The substring match is the fallback for that case.
        let e = io::Error::other("peer closed without sending close_notify alert");
        assert!(is_tls_close_notify(&e));
    }

    #[test]
    fn is_tls_close_notify_rejects_unrelated_errors() {
        // ConnectionReset / InvalidData must NOT be swallowed: those are
        // real failures the caller needs to see.
        let reset = io::Error::new(io::ErrorKind::ConnectionReset, "reset by peer");
        assert!(!is_tls_close_notify(&reset));

        let tls = io::Error::new(io::ErrorKind::InvalidData, "bad certificate");
        assert!(!is_tls_close_notify(&tls));

        // Other kind with a benign message must not match either — only the
        // exact substring `close_notify` triggers the string fallback.
        let benign = io::Error::other("operation successful");
        assert!(!is_tls_close_notify(&benign));
    }

    #[test]
    fn is_tls_close_notify_string_match_is_case_sensitive() {
        // The implementation does a literal `contains("close_notify")`. This
        // test pins that contract so a future case-insensitive refactor is a
        // conscious decision, not an accident.
        let upper = io::Error::other("Close_Notify received");
        assert!(!is_tls_close_notify(&upper));
    }
}
