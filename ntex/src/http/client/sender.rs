use std::convert::TryFrom;
use std::error::Error;
use std::net;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use derive_more::From;
use futures::{Future, Stream};
use serde::Serialize;

use crate::http::body::{Body, BodyStream};
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::RequestHeadType;
use crate::rt::time::{delay_for, Delay};

#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;
#[cfg(feature = "compress")]
use crate::http::header::ContentEncoding;
#[cfg(feature = "compress")]
use crate::http::{Payload, PayloadStream};

use super::error::{FreezeRequestError, InvalidUrl, SendRequestError};
use super::response::ClientResponse;
use super::ClientConfig;

#[derive(Debug, From)]
pub(crate) enum PrepForSendingError {
    Url(InvalidUrl),
    Http(HttpError),
}

impl Into<FreezeRequestError> for PrepForSendingError {
    fn into(self) -> FreezeRequestError {
        match self {
            PrepForSendingError::Url(e) => FreezeRequestError::Url(e),
            PrepForSendingError::Http(e) => FreezeRequestError::Http(e),
        }
    }
}

impl Into<SendRequestError> for PrepForSendingError {
    fn into(self) -> SendRequestError {
        match self {
            PrepForSendingError::Url(e) => SendRequestError::Url(e),
            PrepForSendingError::Http(e) => SendRequestError::Http(e),
        }
    }
}

/// Future that sends request's payload and resolves to a server response.
#[must_use = "futures do nothing unless polled"]
pub enum SendClientRequest {
    Fut(
        Pin<Box<dyn Future<Output = Result<ClientResponse, SendRequestError>>>>,
        Option<Delay>,
        bool,
    ),
    Err(Option<SendRequestError>),
}

impl SendClientRequest {
    pub(crate) fn new(
        send: Pin<Box<dyn Future<Output = Result<ClientResponse, SendRequestError>>>>,
        response_decompress: bool,
        timeout: Option<Duration>,
    ) -> SendClientRequest {
        let delay = timeout.map(delay_for);
        SendClientRequest::Fut(send, delay, response_decompress)
    }
}

impl Future for SendClientRequest {
    #[cfg(feature = "compress")]
    type Output =
        Result<ClientResponse<Decoder<Payload<PayloadStream>>>, SendRequestError>;
    #[cfg(not(feature = "compress"))]
    type Output = Result<ClientResponse, SendRequestError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match this {
            SendClientRequest::Fut(send, delay, _response_decompress) => {
                if delay.is_some() {
                    match Pin::new(delay.as_mut().unwrap()).poll(cx) {
                        Poll::Pending => (),
                        _ => return Poll::Ready(Err(SendRequestError::Timeout)),
                    }
                }

                let res = futures::ready!(Pin::new(send).poll(cx));

                #[cfg(feature = "compress")]
                let res = res.map(|res| {
                    res.map_body(|head, payload| {
                        if *_response_decompress {
                            Payload::Stream(Decoder::from_headers(
                                payload,
                                &head.headers,
                            ))
                        } else {
                            Payload::Stream(Decoder::new(
                                payload,
                                ContentEncoding::Identity,
                            ))
                        }
                    })
                });

                Poll::Ready(res)
            }
            SendClientRequest::Err(ref mut e) => match e.take() {
                Some(e) => Poll::Ready(Err(e)),
                None => panic!("Attempting to call completed future"),
            },
        }
    }
}

impl From<SendRequestError> for SendClientRequest {
    fn from(e: SendRequestError) -> Self {
        SendClientRequest::Err(Some(e))
    }
}

impl From<HttpError> for SendClientRequest {
    fn from(e: HttpError) -> Self {
        SendClientRequest::Err(Some(e.into()))
    }
}

impl From<PrepForSendingError> for SendClientRequest {
    fn from(e: PrepForSendingError) -> Self {
        SendClientRequest::Err(Some(e.into()))
    }
}

impl RequestHeadType {
    pub(super) fn send_body<B>(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Option<Duration>,
        config: &ClientConfig,
        body: B,
    ) -> SendClientRequest
    where
        B: Into<Body>,
    {
        SendClientRequest::new(
            config.connector.send_request(self, body.into(), addr),
            response_decompress,
            timeout.or_else(|| config.timeout),
        )
    }

    pub(super) fn send_json<T: Serialize>(
        mut self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Option<Duration>,
        config: &ClientConfig,
        value: &T,
    ) -> SendClientRequest {
        let body = match serde_json::to_string(value) {
            Ok(body) => body,
            Err(e) => return SendRequestError::Error(Box::new(e)).into(),
        };

        if let Err(e) = self.set_header_if_none(header::CONTENT_TYPE, "application/json")
        {
            return e.into();
        }

        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::Bytes(Bytes::from(body)),
        )
    }

    pub(super) fn send_form<T: Serialize>(
        mut self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Option<Duration>,
        config: &ClientConfig,
        value: &T,
    ) -> SendClientRequest {
        let body = match serde_urlencoded::to_string(value) {
            Ok(body) => body,
            Err(e) => return SendRequestError::Error(Box::new(e)).into(),
        };

        // set content-type
        if let Err(e) = self.set_header_if_none(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        ) {
            return e.into();
        }

        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::Bytes(Bytes::from(body)),
        )
    }

    pub(super) fn send_stream<S, E>(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Option<Duration>,
        config: &ClientConfig,
        stream: S,
    ) -> SendClientRequest
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::from_message(BodyStream::new(stream)),
        )
    }

    pub(super) fn send(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Option<Duration>,
        config: &ClientConfig,
    ) -> SendClientRequest {
        self.send_body(addr, response_decompress, timeout, config, Body::Empty)
    }

    fn set_header_if_none<V>(
        &mut self,
        key: HeaderName,
        value: V,
    ) -> Result<(), HttpError>
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match self {
            RequestHeadType::Owned(head) => {
                if !head.headers.contains_key(&key) {
                    match HeaderValue::try_from(value) {
                        Ok(value) => head.headers.insert(key, value),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            RequestHeadType::Rc(head, extra_headers) => {
                if !head.headers.contains_key(&key)
                    && !extra_headers.iter().any(|h| h.contains_key(&key))
                {
                    match HeaderValue::try_from(value) {
                        Ok(v) => {
                            let h = extra_headers.get_or_insert(HeaderMap::new());
                            h.insert(key, v)
                        }
                        Err(e) => return Err(e.into()),
                    };
                }
            }
        }

        Ok(())
    }
}
