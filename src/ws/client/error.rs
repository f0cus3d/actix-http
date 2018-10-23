//! Http client request
use std::io;

use actix_net::connector::ConnectorError;
use http::header::HeaderValue;
use http::StatusCode;

use error::ParseError;
use http::Error as HttpError;
use ws::ProtocolError;

/// Websocket client error
#[derive(Fail, Debug)]
pub enum ClientError {
    /// Invalid url
    #[fail(display = "Invalid url")]
    InvalidUrl,
    /// Invalid response status
    #[fail(display = "Invalid response status")]
    InvalidResponseStatus(StatusCode),
    /// Invalid upgrade header
    #[fail(display = "Invalid upgrade header")]
    InvalidUpgradeHeader,
    /// Invalid connection header
    #[fail(display = "Invalid connection header")]
    InvalidConnectionHeader(HeaderValue),
    /// Missing CONNECTION header
    #[fail(display = "Missing CONNECTION header")]
    MissingConnectionHeader,
    /// Missing SEC-WEBSOCKET-ACCEPT header
    #[fail(display = "Missing SEC-WEBSOCKET-ACCEPT header")]
    MissingWebSocketAcceptHeader,
    /// Invalid challenge response
    #[fail(display = "Invalid challenge response")]
    InvalidChallengeResponse(String, HeaderValue),
    /// Http parsing error
    #[fail(display = "Http parsing error")]
    Http(HttpError),
    // /// Url parsing error
    // #[fail(display = "Url parsing error")]
    // Url(UrlParseError),
    /// Response parsing error
    #[fail(display = "Response parsing error")]
    ParseError(ParseError),
    /// Protocol error
    #[fail(display = "{}", _0)]
    Protocol(#[cause] ProtocolError),
    /// Connect error
    #[fail(display = "{:?}", _0)]
    Connect(ConnectorError),
    /// IO Error
    #[fail(display = "{}", _0)]
    Io(io::Error),
    /// "Disconnected"
    #[fail(display = "Disconnected")]
    Disconnected,
}

impl From<HttpError> for ClientError {
    fn from(err: HttpError) -> ClientError {
        ClientError::Http(err)
    }
}

impl From<ConnectorError> for ClientError {
    fn from(err: ConnectorError) -> ClientError {
        ClientError::Connect(err)
    }
}

impl From<ProtocolError> for ClientError {
    fn from(err: ProtocolError) -> ClientError {
        ClientError::Protocol(err)
    }
}

impl From<io::Error> for ClientError {
    fn from(err: io::Error) -> ClientError {
        ClientError::Io(err)
    }
}

impl From<ParseError> for ClientError {
    fn from(err: ParseError) -> ClientError {
        ClientError::ParseError(err)
    }
}