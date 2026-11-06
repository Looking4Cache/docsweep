//! IMAP access with password login.
//!
//! [`ImapMailbox::connect_read_only`] uses `EXAMINE` and returns a type without
//! any writing methods. All fetches use `BODY.PEEK`, so `\Seen` flags stay as
//! they are in real runs too.

use std::fmt;
use std::sync::{Arc, Once};
use std::time::Duration;

use async_imap::types::{Capabilities, Name, NameAttribute};
use async_imap::{Client, Session};
use futures_util::TryStreamExt;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::ConfigVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use super::{MailError, MailReader, MailWriter};
use crate::secrets::{ExposeSecret, SecretString};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const STARTTLS_PORT: u16 = 143;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSecurity {
    /// TLS from the first byte.
    Tls,
    /// Plain connection upgraded by `STARTTLS`; failure to upgrade is an error.
    StartTls,
    /// Unencrypted. Exists only for tests against a local server: the
    /// configuration offers no way to select it.
    PlainForTests,
}

impl TransportSecurity {
    /// The security used for a configured port. Never unencrypted.
    pub fn for_port(port: u16) -> Self {
        if port == STARTTLS_PORT {
            Self::StartTls
        } else {
            Self::Tls
        }
    }
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> AsyncStream for T {}
type BoxedStream = Box<dyn AsyncStream>;

fn error(context: &str, cause: impl fmt::Display) -> MailError {
    MailError(format!("{context}: {cause}"))
}

struct Connection {
    session: Session<BoxedStream>,
    /// Hierarchy separator of the server, often `/` or `.`.
    delimiter: String,
    capabilities: Capabilities,
}

impl Connection {
    async fn open(
        host: &str,
        port: u16,
        security: TransportSecurity,
        user: &str,
        password: &SecretString,
    ) -> Result<Self, MailError> {
        let client = timeout(CONNECT_TIMEOUT, connect_client(host, port, security))
            .await
            .map_err(|_| MailError(format!("connection to {host}:{port} timed out")))??;
        let mut session = client
            .login(user, password.expose_secret())
            .await
            .map_err(|(cause, _)| error(&format!("login as {user} on {host} failed"), cause))?;
        let capabilities = session
            .capabilities()
            .await
            .map_err(|cause| error("cannot read server capabilities", cause))?;
        let delimiter = {
            let names: Vec<Name> = session
                .list(Some(""), Some(""))
                .await
                .map_err(|cause| error("cannot list folders", cause))?
                .try_collect()
                .await
                .map_err(|cause| error("cannot list folders", cause))?;
            names
                .first()
                .and_then(|name| name.delimiter().map(str::to_owned))
                .unwrap_or_else(|| "/".to_owned())
        };
        Ok(Self {
            session,
            delimiter,
            capabilities,
        })
    }

    async fn list_headers(&mut self) -> Result<Vec<(u32, Vec<u8>)>, MailError> {
        let fetches: Vec<_> = self
            .session
            .uid_fetch("1:*", "(UID BODY.PEEK[HEADER])")
            .await
            .map_err(|cause| error("cannot list mails", cause))?
            .try_collect()
            .await
            .map_err(|cause| error("cannot list mails", cause))?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| Some((fetch.uid?, fetch.header()?.to_vec())))
            .collect())
    }

    async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>, MailError> {
        let context = || format!("cannot fetch mail {uid}");
        let fetches: Vec<_> = self
            .session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .await
            .map_err(|cause| error(&context(), cause))?
            .try_collect()
            .await
            .map_err(|cause| error(&context(), cause))?;
        fetches
            .iter()
            .find_map(|fetch| fetch.body().map(<[u8]>::to_vec))
            .ok_or_else(|| MailError(format!("{}: the server returned no content", context())))
    }

    async fn logout(&mut self) {
        // The work is done at this point, and a failed goodbye changes nothing.
        let _ = self.session.logout().await;
    }
}

async fn connect_client(
    host: &str,
    port: u16,
    security: TransportSecurity,
) -> Result<Client<BoxedStream>, MailError> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|cause| error(&format!("cannot connect to {host}:{port}"), cause))?;
    let greeting_error = |cause: String| error(&format!("no IMAP greeting from {host}"), cause);

    let stream: BoxedStream = match security {
        TransportSecurity::Tls => Box::new(tls_wrap(host, tcp).await?),
        TransportSecurity::PlainForTests => Box::new(tcp),
        TransportSecurity::StartTls => {
            let mut client = Client::new(tcp);
            read_greeting(&mut client).await.map_err(greeting_error)?;
            client
                .run_command_and_check_ok("STARTTLS", None)
                .await
                .map_err(|cause| error(&format!("{host} refused STARTTLS"), cause))?;
            let tcp = client.into_inner();
            let tls = tls_wrap(host, tcp).await?;
            // After the upgrade the server sends no second greeting.
            return Ok(Client::new(Box::new(tls)));
        }
    };

    let mut client = Client::new(stream);
    read_greeting(&mut client).await.map_err(greeting_error)?;
    Ok(client)
}

async fn read_greeting<T>(client: &mut Client<T>) -> Result<(), String>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug,
{
    match client.read_response().await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("connection closed".to_owned()),
        Err(cause) => Err(cause.to_string()),
    }
}

async fn tls_wrap(
    host: &str,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, MailError> {
    static INSTALL_PROVIDER: Once = Once::new();
    INSTALL_PROVIDER.call_once(|| {
        // Fails only if another part of the process has already chosen one.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let config = ClientConfig::with_platform_verifier()
        .map_err(|cause| error("cannot load the system's certificates", cause))?;
    let name = ServerName::try_from(host.to_owned())
        .map_err(|cause| error(&format!("invalid server name {host}"), cause))?;
    TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|cause| error(&format!("TLS connection to {host} failed"), cause))
}

/// A connected mailbox.
pub struct ImapMailbox;

impl ImapMailbox {
    /// Opens a folder so that nothing can be changed.
    pub async fn connect_read_only(
        host: &str,
        port: u16,
        security: TransportSecurity,
        user: &str,
        password: &SecretString,
        folder: &str,
    ) -> Result<ImapReader, MailError> {
        let mut connection = Connection::open(host, port, security, user, password).await?;
        let encoded = encode_folder(folder, &connection.delimiter);
        let mailbox = connection
            .session
            .examine(&encoded)
            .await
            .map_err(|cause| error(&format!("cannot open folder {folder}"), cause))?;
        Ok(ImapReader {
            connection,
            is_empty: mailbox.exists == 0,
        })
    }

    /// Opens a folder for processing.
    pub async fn connect_writable(
        host: &str,
        port: u16,
        security: TransportSecurity,
        user: &str,
        password: &SecretString,
        folder: &str,
    ) -> Result<ImapWriter, MailError> {
        let mut connection = Connection::open(host, port, security, user, password).await?;
        let encoded = encode_folder(folder, &connection.delimiter);
        let mailbox = connection
            .session
            .select(&encoded)
            .await
            .map_err(|cause| error(&format!("cannot open folder {folder}"), cause))?;
        Ok(ImapWriter {
            connection,
            is_empty: mailbox.exists == 0,
        })
    }
}

/// A folder opened with `EXAMINE`.
pub struct ImapReader {
    connection: Connection,
    is_empty: bool,
}

impl ImapReader {
    /// Ends the session.
    pub async fn logout(&mut self) {
        self.connection.logout().await;
    }
}

impl MailReader for ImapReader {
    async fn list_headers(&mut self) -> Result<Vec<(u32, Vec<u8>)>, MailError> {
        if self.is_empty {
            return Ok(Vec::new());
        }
        self.connection.list_headers().await
    }

    async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>, MailError> {
        self.connection.fetch_raw(uid).await
    }
}

/// A folder opened with `SELECT`.
pub struct ImapWriter {
    connection: Connection,
    is_empty: bool,
}

impl ImapWriter {
    /// Ends the session.
    pub async fn logout(&mut self) {
        self.connection.logout().await;
    }

    fn has_capability(&self, name: &str) -> bool {
        self.connection.capabilities.has_str(name)
    }

    /// Finds the folder the server marks with the `\Trash` attribute.
    async fn trash_folder(&mut self) -> Result<Option<String>, MailError> {
        let names: Vec<Name> = self
            .connection
            .session
            .list(Some(""), Some("*"))
            .await
            .map_err(|cause| error("cannot list folders", cause))?
            .try_collect()
            .await
            .map_err(|cause| error("cannot list folders", cause))?;
        Ok(names
            .iter()
            .find(|name| name.attributes().contains(&NameAttribute::Trash))
            .map(|name| name.name().to_owned()))
    }

    async fn ensure_folder(&mut self, encoded: &str) -> Result<(), MailError> {
        let existing: Vec<Name> = self
            .connection
            .session
            .list(Some(""), Some(encoded))
            .await
            .map_err(|cause| error("cannot list folders", cause))?
            .try_collect()
            .await
            .map_err(|cause| error("cannot list folders", cause))?;
        if existing.is_empty() {
            self.connection
                .session
                .create(encoded)
                .await
                .map_err(|cause| error(&format!("cannot create folder {encoded}"), cause))?;
        }
        Ok(())
    }

    /// Moves to an already encoded folder name.
    async fn move_encoded(&mut self, uid: u32, encoded: &str) -> Result<(), MailError> {
        let context = format!("cannot move mail {uid} to {encoded}");
        if self.has_capability("MOVE") {
            return self
                .connection
                .session
                .uid_mv(uid.to_string(), encoded)
                .await
                .map_err(|cause| error(&context, cause));
        }
        // Checked before copying, so that failing here cannot leave two copies.
        self.require_uidplus(&context)?;
        self.connection
            .session
            .uid_copy(uid.to_string(), encoded)
            .await
            .map_err(|cause| error(&context, cause))?;
        self.remove_single(uid).await
    }

    fn require_uidplus(&self, context: &str) -> Result<(), MailError> {
        if self.has_capability("UIDPLUS") {
            Ok(())
        } else {
            // A plain EXPUNGE would also remove mails others marked as deleted.
            Err(MailError(format!(
                "{context}: the server supports neither MOVE nor UIDPLUS"
            )))
        }
    }

    /// Removes exactly one mail through `UID EXPUNGE`.
    async fn remove_single(&mut self, uid: u32) -> Result<(), MailError> {
        let context = format!("cannot remove mail {uid}");
        let _: Vec<_> = self
            .connection
            .session
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .await
            .map_err(|cause| error(&context, cause))?
            .try_collect()
            .await
            .map_err(|cause| error(&context, cause))?;
        let _: Vec<_> = self
            .connection
            .session
            .uid_expunge(uid.to_string())
            .await
            .map_err(|cause| error(&context, cause))?
            .try_collect()
            .await
            .map_err(|cause| error(&context, cause))?;
        Ok(())
    }
}

impl MailReader for ImapWriter {
    async fn list_headers(&mut self) -> Result<Vec<(u32, Vec<u8>)>, MailError> {
        if self.is_empty {
            return Ok(Vec::new());
        }
        self.connection.list_headers().await
    }

    async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>, MailError> {
        self.connection.fetch_raw(uid).await
    }
}

impl MailWriter for ImapWriter {
    async fn move_to(&mut self, uid: u32, folder: &str) -> Result<(), MailError> {
        let encoded = encode_folder(folder, &self.connection.delimiter);
        self.ensure_folder(&encoded).await?;
        self.move_encoded(uid, &encoded).await
    }

    async fn move_to_trash(&mut self, uid: u32) -> Result<(), MailError> {
        let Some(trash) = self.trash_folder().await? else {
            return Err(MailError(
                "the server names no trash folder (SPECIAL-USE), so the mail stays in place"
                    .to_owned(),
            ));
        };
        self.move_encoded(uid, &trash).await
    }

    async fn delete_permanently(&mut self, uid: u32) -> Result<(), MailError> {
        self.require_uidplus(&format!("cannot delete mail {uid}"))?;
        self.remove_single(uid).await
    }

    async fn flag(&mut self, uid: u32) -> Result<(), MailError> {
        let context = format!("cannot flag mail {uid}");
        let _: Vec<_> = self
            .connection
            .session
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Flagged)")
            .await
            .map_err(|cause| error(&context, cause))?
            .try_collect()
            .await
            .map_err(|cause| error(&context, cause))?;
        Ok(())
    }
}

/// Converts a configured folder (levels separated by `/`) for the wire: the
/// server's separator and modified UTF-7 for non-ASCII names.
fn encode_folder(folder: &str, delimiter: &str) -> String {
    let joined = folder
        .split('/')
        .map(|level| utf7_imap::encode_utf7_imap(level.to_owned()))
        .collect::<Vec<_>>()
        .join(delimiter);
    if joined.eq_ignore_ascii_case("inbox") {
        "INBOX".to_owned()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_ports_never_select_an_unencrypted_connection() {
        assert_eq!(TransportSecurity::for_port(993), TransportSecurity::Tls);
        assert_eq!(
            TransportSecurity::for_port(143),
            TransportSecurity::StartTls
        );
        for port in 0..=u16::MAX {
            assert_ne!(
                TransportSecurity::for_port(port),
                TransportSecurity::PlainForTests
            );
        }
    }

    #[test]
    fn folders_use_the_server_separator_and_encoding() {
        assert_eq!(
            encode_folder("Rechnungen/Erledigt", "."),
            "Rechnungen.Erledigt"
        );
        assert_eq!(
            encode_folder("Rechnungen/Erledigt", "/"),
            "Rechnungen/Erledigt"
        );
        assert_eq!(encode_folder("Büro", "/"), "B&APw-ro");
        assert_eq!(encode_folder("inbox", "/"), "INBOX");
    }

    /// Needs internet access, so it only runs on request. A handshake is all it
    /// does: it verifies the system's certificate chain and sends no credentials.
    #[tokio::test]
    #[ignore = "needs internet access"]
    async fn tls_handshake_succeeds_with_a_public_server() {
        let host = "github.com";
        let tcp = TcpStream::connect((host, 443)).await.unwrap();
        tls_wrap(host, tcp).await.unwrap();
    }

    #[test]
    fn tls_stack_can_be_set_up() {
        // Installing the provider and loading system certificates must work on
        // every platform in the build matrix.
        let _ = rustls::crypto::ring::default_provider().install_default();
        ClientConfig::with_platform_verifier().unwrap();
    }
}
