mod http;
// mod ws;

use std::{
    io,
    net::IpAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

pub use crate::http::*;
use async_net::TcpStream;
use futures::{AsyncRead, AsyncWrite};
use futures_rustls::{
    client::TlsStream,
    rustls::{ClientConfig, RootCertStore},
    TlsConnector,
};
use rustls_pki_types::{InvalidDnsNameError, ServerName, TrustAnchor};
// pub use ws::*;

pub enum Transport {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl Transport {
    async fn connect(tls: Option<Arc<ClientConfig>>, host: &str, port: u16) -> Result<Self, TransportError> {
        let server = ServerName::try_from(host)
            .map_err(|err| TransportError::InvalidDnsName(Arc::new(err)))?
            .to_owned();
        let tcp = match &server {
            ServerName::DnsName(name) => TcpStream::connect((name.as_ref(), port)).await,
            ServerName::IpAddress(ip) => TcpStream::connect((IpAddr::from(*ip), port)).await,
            _ => unreachable!(),
        }
        .map_err(|err| TransportError::TcpConnect(Arc::new(err)))?;
        let transport = match tls {
            None => Transport::Tcp(tcp),
            Some(client_config) => {
                let tls = TlsConnector::from(client_config)
                    .connect(server, tcp)
                    .await
                    .map_err(|err| TransportError::TlsConnect(Arc::new(err)))?;
                Transport::Tls(tls)
            }
        };
        Ok(transport)
    }
}

impl Unpin for Transport {}

impl AsyncRead for Transport {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Transport::Tcp(tcp) => Pin::new(tcp).poll_read(cx, buf),
            Transport::Tls(tls) => Pin::new(tls).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Transport::Tcp(tcp) => Pin::new(tcp).poll_write(cx, buf),
            Transport::Tls(tls) => Pin::new(tls).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(tcp) => Pin::new(tcp).poll_flush(cx),
            Transport::Tls(tls) => Pin::new(tls).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(tcp) => Pin::new(tcp).poll_close(cx),
            Transport::Tls(tls) => Pin::new(tls).poll_close(cx),
        }
    }
}

use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum TransportError {
    #[error("invalid host name: {0:?}")]
    InvalidDnsName(Arc<InvalidDnsNameError>),
    #[error("tcp connect error: {0:?}")]
    TcpConnect(Arc<io::Error>),
    #[error("tls connect error: {0:?}")]
    TlsConnect(Arc<io::Error>),
}

lazy_static::lazy_static! {
    pub (crate) static ref DEFAULT_CLIENT_CONFIG: Arc<ClientConfig> = {
        let roots = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .map(|t| {TrustAnchor{subject: t.subject.into(), subject_public_key_info: t.spki.into() , name_constraints: t.name_constraints.map(Into::into)}});
        let mut root_store = RootCertStore::empty();
        root_store.extend(roots);
        let config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Arc::new(config)
    };
}
