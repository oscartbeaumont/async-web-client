use std::borrow::Cow;
use std::io;
use std::mem::replace;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_http_codec::internal::buffer_decode::BufferDecodeState;
use async_http_codec::internal::buffer_write::BufferWriteState;
use async_http_codec::internal::io_future::{IoFutureState, IoFutureWithOutputState};
use async_http_codec::{BodyEncodeState, RequestHead, ResponseHead};
use async_net::TcpStream;
use futures::future::poll_fn;
use futures::{ready, AsyncWrite, Future};
use http::header::TRANSFER_ENCODING;
use http::uri::Scheme;
use http::{HeaderMap, HeaderValue, Method, Response, Uri, Version};

use super::common::extract_origin;
use super::error::HttpError;
use super::response_native::ResponseRead;

pub enum RequestSend<'a> {
    Start {
        body: &'a [u8],
        method: Method,
        uri: &'a Uri,
        headers: &'a HeaderMap,
    },
    PendingConnect {
        body: &'a [u8],
        method: Method,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        transport: Pin<Box<dyn Future<Output = io::Result<TcpStream>>>>,
    },
    SendingHead {
        body: &'a [u8],
        write_state: BufferWriteState,
        transport: TcpStream,
    },
    SendingBody {
        body: &'a [u8],
        remaining: &'a [u8],
        write_state: BodyEncodeState,
        transport: TcpStream,
    },
    Flushing {
        transport: TcpStream,
    },
    ReceivingHead {
        transport: TcpStream,
        dec_state: BufferDecodeState<ResponseHead<'static>>,
    },
    Finished,
}

impl RequestSend<'_> {
    pub fn new(request: &http::Request<impl AsRef<[u8]>>) -> RequestSend<'_> {
        let body = request.body().as_ref();
        let uri = request.uri();
        let headers = request.headers();
        let method = request.method().clone();
        RequestSend::Start { method, body, uri, headers }
    }
    pub fn poll(&mut self, cx: &mut Context) -> Poll<Result<http::Response<ResponseRead>, HttpError>> {
        loop {
            let s = replace(self, RequestSend::Finished);
            match s {
                RequestSend::Start { method, body, uri, headers } => {
                    let (scheme, host, port) = extract_origin(uri, headers)?;
                    let https = match scheme {
                        _ if scheme == Some(Scheme::HTTP) => false,
                        _ if scheme == Some(Scheme::HTTPS) => true,
                        None => true,
                        Some(scheme) => return Poll::Ready(Err(HttpError::UnexpectedScheme(scheme))),
                    };
                    let addr = (
                        host.to_string(),
                        port.unwrap_or(match https {
                            true => 443,
                            false => 80,
                        }),
                    );
                    *self = RequestSend::PendingConnect {
                        body,
                        transport: Box::pin(TcpStream::connect(addr)),
                        method,
                        uri,
                        headers,
                    }
                }
                RequestSend::PendingConnect {
                    body,
                    mut transport,
                    method,
                    uri,
                    headers,
                } => match transport.as_mut().poll(cx) {
                    Poll::Ready(Ok(transport)) => {
                        let (_scheme, host, port) = extract_origin(uri, headers)?;
                        let mut head = RequestHead::new(method, Cow::Borrowed(uri), Version::HTTP_11, Cow::Borrowed(headers));
                        if head.headers().get(http::header::HOST).is_none() {
                            let host = match port {
                                Some(port) => HeaderValue::from_str(&format!("{}:{}", host, port)).unwrap(),
                                None => HeaderValue::from_str(&host).unwrap(),
                            };
                            head.headers_mut().insert(http::header::HOST, host);
                        }
                        if head.headers().get(http::header::CONTENT_LENGTH).is_none() {
                            let length = HeaderValue::from_str(&format!("{}", body.len())).unwrap();
                            head.headers_mut().insert(http::header::CONTENT_LENGTH, length);
                        }
                        let write_state = head.encode_state();
                        *self = RequestSend::SendingHead {
                            write_state,
                            transport,
                            body,
                        };
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(HttpError::ConnectError(Arc::new(err)))),
                    Poll::Pending => {
                        *self = RequestSend::PendingConnect {
                            body,
                            method,
                            uri,
                            headers,
                            transport,
                        };
                        return Poll::Pending;
                    }
                },
                RequestSend::SendingHead {
                    mut write_state,
                    mut transport,
                    body,
                } => match write_state.poll(cx, &mut transport) {
                    Poll::Ready(Ok(())) => {
                        let write_state = BodyEncodeState::new(Some(body.len() as u64));
                        let remaining = body;
                        *self = RequestSend::SendingBody {
                            body,
                            write_state,
                            transport,
                            remaining,
                        }
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(HttpError::IoError(Arc::new(err)))),
                    Poll::Pending => {
                        *self = RequestSend::SendingHead {
                            write_state,
                            transport,
                            body,
                        };
                        return Poll::Pending;
                    }
                },
                RequestSend::SendingBody {
                    mut write_state,
                    mut transport,
                    body,
                    mut remaining,
                } => match write_state.poll_write(&mut transport, cx, remaining) {
                    Poll::Ready(Ok(n)) => {
                        remaining = &remaining[n..];
                        match remaining.len() {
                            0 => *self = RequestSend::Flushing { transport },
                            _ => {
                                *self = RequestSend::SendingBody {
                                    write_state,
                                    transport,
                                    body,
                                    remaining,
                                }
                            }
                        }
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(HttpError::IoError(Arc::new(err)))),
                    Poll::Pending => {
                        *self = RequestSend::SendingBody {
                            write_state,
                            transport,
                            body,
                            remaining,
                        };
                        return Poll::Pending;
                    }
                },
                RequestSend::Flushing { mut transport } => match Pin::new(&mut transport).poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        let dec_state = ResponseHead::decode_state();
                        *self = RequestSend::ReceivingHead { dec_state, transport }
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(HttpError::IoError(Arc::new(err)))),
                    Poll::Pending => {
                        *self = RequestSend::Flushing { transport };
                        return Poll::Pending;
                    }
                },
                RequestSend::ReceivingHead {
                    mut dec_state,
                    mut transport,
                } => match dec_state.poll(cx, &mut transport) {
                    Poll::Ready(Ok(head)) => {
                        let body = ResponseRead::new(transport, &head)?;
                        let parts: http::response::Parts = head.into();
                        return Poll::Ready(Ok(Response::from_parts(parts, body)));
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(HttpError::IoError(Arc::new(err)))),
                    Poll::Pending => {
                        *self = RequestSend::ReceivingHead { transport, dec_state };
                        return Poll::Pending;
                    }
                },
                RequestSend::Finished => panic!("polled finished future"),
            }
        }
    }
    pub fn is_terminated(&self) -> bool {
        match self {
            RequestSend::Finished => true,
            _ => false,
        }
    }
}

pub struct RequestWrite {
    error: Option<HttpError>,
    pending_connect: Option<Pin<Box<dyn Future<Output = io::Result<TcpStream>>>>>,
    pending_head: Option<BufferWriteState>,
    transport: Option<TcpStream>,
    body_encode_state: Option<BodyEncodeState>,
}

impl RequestWrite {
    pub fn start<T>(request: &http::Request<T>) -> Self {
        let https = match request.uri().scheme() {
            Some(scheme) => match scheme {
                _ if scheme == &Scheme::HTTP => false,
                _ if scheme == &Scheme::HTTPS => true,
                scheme => return Self::error(HttpError::UnexpectedScheme(scheme.clone())),
            },
            None => true,
        };
        let host = match request.uri().host() {
            Some(host) => host.to_string(),
            None => return Self::error(HttpError::MissingHost),
        };
        let port = match request.uri().port_u16() {
            Some(port) => port,
            None => match https {
                true => 443u16,
                false => 80u16,
            },
        };
        let mut head = RequestHead::ref_request(request);
        head.headers_mut().insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        Self {
            error: None,
            pending_connect: Some(Box::pin(TcpStream::connect((host, port)))),
            pending_head: Some(head.encode_state()),
            transport: None,
            body_encode_state: Some(BodyEncodeState::new(None)),
        }
    }
    pub async fn response(mut self) -> Result<(http::Response<()>, ResponseRead), HttpError> {
        if let Err(_) = poll_fn(|cx| Pin::new(&mut self).poll_close(cx)).await {
            return Err(self.error.unwrap().clone());
        }
        let t = self.transport.take().unwrap();
        let (t, head) = match ResponseHead::decode(t).await {
            Ok((t, head)) => (t, head),
            Err(err) => return Err(HttpError::IoError(err.into())), // TODO: better errors upstream
        };
        let resp = ResponseRead::new(t, &head)?;
        Ok((head.into(), resp))
    }
    fn error(err: HttpError) -> Self {
        Self {
            error: Some(err),
            pending_connect: None,
            pending_head: None,
            transport: None,
            body_encode_state: None,
        }
    }
    fn poll_before_body(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), HttpError>> {
        if let Some(fut) = &mut self.pending_connect {
            match ready!(fut.as_mut().poll(cx)) {
                Ok(transport) => {
                    self.transport = Some(transport);
                    self.pending_connect = None;
                }
                Err(err) => {
                    let err = HttpError::ConnectError(Arc::new(err));
                    *self = Self::error(err.clone());
                    return Poll::Ready(Err(err));
                }
            }
        }
        let transport = self.transport.as_mut().unwrap();
        if let Some(state) = &mut self.pending_head {
            match ready!(state.poll(cx, transport)) {
                Ok(()) => self.pending_head = None,
                Err(err) => {
                    let err = HttpError::IoError(Arc::new(err));
                    *self = Self::error(err.clone());
                    return Poll::Ready(Err(err));
                }
            }
        }
        Poll::Ready(Ok(()))
    }
    fn already_closed() -> io::Error {
        io::Error::new(io::ErrorKind::NotConnected, "already closed")
    }
}

impl AsyncWrite for RequestWrite {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if let Some(err) = self.error.clone() {
            return Poll::Ready(Err(err.into()));
        }
        if let Err(err) = ready!(self.poll_before_body(cx)) {
            return Poll::Ready(Err(err.into()));
        }
        let t = match self.transport.take() {
            Some(t) => t,
            None => return Poll::Ready(Err(Self::already_closed())),
        };
        let mut w = self.body_encode_state.take().unwrap().into_async_write(t);
        let p = match Pin::new(&mut w).poll_write(cx, buf) {
            Poll::Ready(Err(err)) => {
                let err = HttpError::IoError(err.into());
                *self = Self::error(err.clone());
                Poll::Ready(Err(err.into()))
            }
            p => p,
        };
        let (t, s) = w.checkpoint();
        self.body_encode_state = Some(s);
        if self.error.is_none() {
            self.transport = Some(t);
        }
        p
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(err) = self.error.clone() {
            return Poll::Ready(Err(err.into()));
        }
        if let Err(err) = ready!(self.poll_before_body(cx)) {
            return Poll::Ready(Err(err.into()));
        }
        let t = match self.transport.take() {
            Some(t) => t,
            None => return Poll::Ready(Err(Self::already_closed())),
        };
        let mut w = self.body_encode_state.take().unwrap().into_async_write(t);
        let p = match Pin::new(&mut w).poll_flush(cx) {
            Poll::Ready(Err(err)) => {
                let err = HttpError::IoError(err.into());
                *self = Self::error(err.clone());
                Poll::Ready(Err(err.into()))
            }
            p => p,
        };
        let (t, s) = w.checkpoint();
        self.body_encode_state = Some(s);
        if self.error.is_none() {
            self.transport = Some(t);
        }
        p
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(err) = self.error.clone() {
            return Poll::Ready(Err(err.into()));
        }
        if let Err(err) = ready!(self.poll_before_body(cx)) {
            return Poll::Ready(Err(err.into()));
        }
        let t = match self.transport.take() {
            Some(t) => t,
            None => return Poll::Ready(Err(Self::already_closed())),
        };
        let mut w = self.body_encode_state.take().unwrap().into_async_write(t);
        let p = match Pin::new(&mut w).poll_close(cx) {
            Poll::Ready(Ok(())) => {
                drop(self.transport.take());
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                let err = HttpError::IoError(err.into());
                *self = Self::error(err.clone());
                Poll::Ready(Err(err.into()))
            }
            p => p,
        };
        let (t, s) = w.checkpoint();
        self.body_encode_state = Some(s);
        if self.error.is_none() {
            self.transport = Some(t);
        }
        p
    }
}
