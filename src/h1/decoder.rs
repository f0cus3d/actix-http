use std::{io, mem};

use bytes::{Bytes, BytesMut};
use futures::{Async, Poll};
use httparse;
use tokio_codec::Decoder;

use error::ParseError;
use http::header::{HeaderName, HeaderValue};
use http::{header, HttpTryFrom, Method, Uri, Version};
use request::{MessageFlags, Request, RequestPool};
use uri::Url;

const MAX_BUFFER_SIZE: usize = 131_072;
const MAX_HEADERS: usize = 96;

pub struct RequestDecoder(&'static RequestPool);

impl RequestDecoder {
    pub(crate) fn with_pool(pool: &'static RequestPool) -> RequestDecoder {
        RequestDecoder(pool)
    }
}

impl Default for RequestDecoder {
    fn default() -> RequestDecoder {
        RequestDecoder::with_pool(RequestPool::pool())
    }
}

impl Decoder for RequestDecoder {
    type Item = (Request, Option<PayloadDecoder>);
    type Error = ParseError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Parse http message
        let mut has_upgrade = false;
        let mut chunked = false;
        let mut content_length = None;

        let msg = {
            // Unsafe: we read only this data only after httparse parses headers into.
            // performance bump for pipeline benchmarks.
            let mut headers: [HeaderIndex; MAX_HEADERS] =
                unsafe { mem::uninitialized() };

            let (len, method, path, version, headers_len) = {
                let mut parsed: [httparse::Header; MAX_HEADERS] =
                    unsafe { mem::uninitialized() };

                let mut req = httparse::Request::new(&mut parsed);
                match req.parse(src)? {
                    httparse::Status::Complete(len) => {
                        let method = Method::from_bytes(req.method.unwrap().as_bytes())
                            .map_err(|_| ParseError::Method)?;
                        let path = Url::new(Uri::try_from(req.path.unwrap())?);
                        let version = if req.version.unwrap() == 1 {
                            Version::HTTP_11
                        } else {
                            Version::HTTP_10
                        };
                        HeaderIndex::record(src, req.headers, &mut headers);

                        (len, method, path, version, req.headers.len())
                    }
                    httparse::Status::Partial => return Ok(None),
                }
            };

            let slice = src.split_to(len).freeze();

            // convert headers
            let mut msg = RequestPool::get(self.0);
            {
                let inner = msg.inner_mut();
                inner
                    .flags
                    .get_mut()
                    .set(MessageFlags::KEEPALIVE, version != Version::HTTP_10);

                for idx in headers[..headers_len].iter() {
                    if let Ok(name) =
                        HeaderName::from_bytes(&slice[idx.name.0..idx.name.1])
                    {
                        // Unsafe: httparse check header value for valid utf-8
                        let value = unsafe {
                            HeaderValue::from_shared_unchecked(
                                slice.slice(idx.value.0, idx.value.1),
                            )
                        };
                        match name {
                            header::CONTENT_LENGTH => {
                                if let Ok(s) = value.to_str() {
                                    if let Ok(len) = s.parse::<u64>() {
                                        content_length = Some(len);
                                    } else {
                                        debug!("illegal Content-Length: {:?}", len);
                                        return Err(ParseError::Header);
                                    }
                                } else {
                                    debug!("illegal Content-Length: {:?}", len);
                                    return Err(ParseError::Header);
                                }
                            }
                            // transfer-encoding
                            header::TRANSFER_ENCODING => {
                                if let Ok(s) = value.to_str() {
                                    chunked = s.to_lowercase().contains("chunked");
                                } else {
                                    return Err(ParseError::Header);
                                }
                            }
                            // connection keep-alive state
                            header::CONNECTION => {
                                let ka = if let Ok(conn) = value.to_str() {
                                    if version == Version::HTTP_10
                                        && conn.contains("keep-alive")
                                    {
                                        true
                                    } else {
                                        version == Version::HTTP_11 && !(conn
                                            .contains("close")
                                            || conn.contains("upgrade"))
                                    }
                                } else {
                                    false
                                };
                                inner.flags.get_mut().set(MessageFlags::KEEPALIVE, ka);
                            }
                            header::UPGRADE => {
                                has_upgrade = true;
                            }
                            _ => (),
                        }

                        inner.headers.append(name, value);
                    } else {
                        return Err(ParseError::Header);
                    }
                }

                inner.url = path;
                inner.method = method;
                inner.version = version;
            }
            msg
        };

        // https://tools.ietf.org/html/rfc7230#section-3.3.3
        let decoder = if chunked {
            // Chunked encoding
            Some(PayloadDecoder::chunked())
        } else if let Some(len) = content_length {
            // Content-Length
            Some(PayloadDecoder::length(len))
        } else if has_upgrade || msg.inner.method == Method::CONNECT {
            // upgrade(websocket) or connect
            Some(PayloadDecoder::eof())
        } else if src.len() >= MAX_BUFFER_SIZE {
            error!("MAX_BUFFER_SIZE unprocessed data reached, closing");
            return Err(ParseError::TooLarge);
        } else {
            None
        };

        Ok(Some((msg, decoder)))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HeaderIndex {
    pub(crate) name: (usize, usize),
    pub(crate) value: (usize, usize),
}

impl HeaderIndex {
    pub(crate) fn record(
        bytes: &[u8], headers: &[httparse::Header], indices: &mut [HeaderIndex],
    ) {
        let bytes_ptr = bytes.as_ptr() as usize;
        for (header, indices) in headers.iter().zip(indices.iter_mut()) {
            let name_start = header.name.as_ptr() as usize - bytes_ptr;
            let name_end = name_start + header.name.len();
            indices.name = (name_start, name_end);
            let value_start = header.value.as_ptr() as usize - bytes_ptr;
            let value_end = value_start + header.value.len();
            indices.value = (value_start, value_end);
        }
    }
}

#[derive(Debug, Clone)]
/// Http payload item
pub enum PayloadItem {
    Chunk(Bytes),
    Eof,
}

/// Decoders to handle different Transfer-Encodings.
///
/// If a message body does not include a Transfer-Encoding, it *should*
/// include a Content-Length header.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadDecoder {
    kind: Kind,
}

impl PayloadDecoder {
    pub fn length(x: u64) -> PayloadDecoder {
        PayloadDecoder {
            kind: Kind::Length(x),
        }
    }

    pub fn chunked() -> PayloadDecoder {
        PayloadDecoder {
            kind: Kind::Chunked(ChunkedState::Size, 0),
        }
    }

    pub fn eof() -> PayloadDecoder {
        PayloadDecoder { kind: Kind::Eof }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// A Reader used when a Content-Length header is passed with a positive
    /// integer.
    Length(u64),
    /// A Reader used when Transfer-Encoding is `chunked`.
    Chunked(ChunkedState, u64),
    /// A Reader used for responses that don't indicate a length or chunked.
    ///
    /// Note: This should only used for `Response`s. It is illegal for a
    /// `Request` to be made with both `Content-Length` and
    /// `Transfer-Encoding: chunked` missing, as explained from the spec:
    ///
    /// > If a Transfer-Encoding header field is present in a response and
    /// > the chunked transfer coding is not the final encoding, the
    /// > message body length is determined by reading the connection until
    /// > it is closed by the server.  If a Transfer-Encoding header field
    /// > is present in a request and the chunked transfer coding is not
    /// > the final encoding, the message body length cannot be determined
    /// > reliably; the server MUST respond with the 400 (Bad Request)
    /// > status code and then close the connection.
    Eof,
}

#[derive(Debug, PartialEq, Clone)]
enum ChunkedState {
    Size,
    SizeLws,
    Extension,
    SizeLf,
    Body,
    BodyCr,
    BodyLf,
    EndCr,
    EndLf,
    End,
}

impl Decoder for PayloadDecoder {
    type Item = PayloadItem;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match self.kind {
            Kind::Length(ref mut remaining) => {
                if *remaining == 0 {
                    Ok(Some(PayloadItem::Eof))
                } else {
                    if src.is_empty() {
                        return Ok(None);
                    }
                    let len = src.len() as u64;
                    let buf;
                    if *remaining > len {
                        buf = src.take().freeze();
                        *remaining -= len;
                    } else {
                        buf = src.split_to(*remaining as usize).freeze();
                        *remaining = 0;
                    };
                    trace!("Length read: {}", buf.len());
                    Ok(Some(PayloadItem::Chunk(buf)))
                }
            }
            Kind::Chunked(ref mut state, ref mut size) => {
                loop {
                    let mut buf = None;
                    // advances the chunked state
                    *state = match state.step(src, size, &mut buf)? {
                        Async::NotReady => return Ok(None),
                        Async::Ready(state) => state,
                    };
                    if *state == ChunkedState::End {
                        trace!("End of chunked stream");
                        return Ok(Some(PayloadItem::Eof));
                    }
                    if let Some(buf) = buf {
                        return Ok(Some(PayloadItem::Chunk(buf)));
                    }
                    if src.is_empty() {
                        return Ok(None);
                    }
                }
            }
            Kind::Eof => {
                if src.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(PayloadItem::Chunk(src.take().freeze())))
                }
            }
        }
    }
}

macro_rules! byte (
    ($rdr:ident) => ({
        if $rdr.len() > 0 {
            let b = $rdr[0];
            $rdr.split_to(1);
            b
        } else {
            return Ok(Async::NotReady)
        }
    })
);

impl ChunkedState {
    fn step(
        &self, body: &mut BytesMut, size: &mut u64, buf: &mut Option<Bytes>,
    ) -> Poll<ChunkedState, io::Error> {
        use self::ChunkedState::*;
        match *self {
            Size => ChunkedState::read_size(body, size),
            SizeLws => ChunkedState::read_size_lws(body),
            Extension => ChunkedState::read_extension(body),
            SizeLf => ChunkedState::read_size_lf(body, size),
            Body => ChunkedState::read_body(body, size, buf),
            BodyCr => ChunkedState::read_body_cr(body),
            BodyLf => ChunkedState::read_body_lf(body),
            EndCr => ChunkedState::read_end_cr(body),
            EndLf => ChunkedState::read_end_lf(body),
            End => Ok(Async::Ready(ChunkedState::End)),
        }
    }
    fn read_size(rdr: &mut BytesMut, size: &mut u64) -> Poll<ChunkedState, io::Error> {
        let radix = 16;
        match byte!(rdr) {
            b @ b'0'...b'9' => {
                *size *= radix;
                *size += u64::from(b - b'0');
            }
            b @ b'a'...b'f' => {
                *size *= radix;
                *size += u64::from(b + 10 - b'a');
            }
            b @ b'A'...b'F' => {
                *size *= radix;
                *size += u64::from(b + 10 - b'A');
            }
            b'\t' | b' ' => return Ok(Async::Ready(ChunkedState::SizeLws)),
            b';' => return Ok(Async::Ready(ChunkedState::Extension)),
            b'\r' => return Ok(Async::Ready(ChunkedState::SizeLf)),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid chunk size line: Invalid Size",
                ));
            }
        }
        Ok(Async::Ready(ChunkedState::Size))
    }
    fn read_size_lws(rdr: &mut BytesMut) -> Poll<ChunkedState, io::Error> {
        trace!("read_size_lws");
        match byte!(rdr) {
            // LWS can follow the chunk size, but no more digits can come
            b'\t' | b' ' => Ok(Async::Ready(ChunkedState::SizeLws)),
            b';' => Ok(Async::Ready(ChunkedState::Extension)),
            b'\r' => Ok(Async::Ready(ChunkedState::SizeLf)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid chunk size linear white space",
            )),
        }
    }
    fn read_extension(rdr: &mut BytesMut) -> Poll<ChunkedState, io::Error> {
        match byte!(rdr) {
            b'\r' => Ok(Async::Ready(ChunkedState::SizeLf)),
            _ => Ok(Async::Ready(ChunkedState::Extension)), // no supported extensions
        }
    }
    fn read_size_lf(
        rdr: &mut BytesMut, size: &mut u64,
    ) -> Poll<ChunkedState, io::Error> {
        match byte!(rdr) {
            b'\n' if *size > 0 => Ok(Async::Ready(ChunkedState::Body)),
            b'\n' if *size == 0 => Ok(Async::Ready(ChunkedState::EndCr)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid chunk size LF",
            )),
        }
    }

    fn read_body(
        rdr: &mut BytesMut, rem: &mut u64, buf: &mut Option<Bytes>,
    ) -> Poll<ChunkedState, io::Error> {
        trace!("Chunked read, remaining={:?}", rem);

        let len = rdr.len() as u64;
        if len == 0 {
            Ok(Async::Ready(ChunkedState::Body))
        } else {
            let slice;
            if *rem > len {
                slice = rdr.take().freeze();
                *rem -= len;
            } else {
                slice = rdr.split_to(*rem as usize).freeze();
                *rem = 0;
            }
            *buf = Some(slice);
            if *rem > 0 {
                Ok(Async::Ready(ChunkedState::Body))
            } else {
                Ok(Async::Ready(ChunkedState::BodyCr))
            }
        }
    }

    fn read_body_cr(rdr: &mut BytesMut) -> Poll<ChunkedState, io::Error> {
        match byte!(rdr) {
            b'\r' => Ok(Async::Ready(ChunkedState::BodyLf)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid chunk body CR",
            )),
        }
    }
    fn read_body_lf(rdr: &mut BytesMut) -> Poll<ChunkedState, io::Error> {
        match byte!(rdr) {
            b'\n' => Ok(Async::Ready(ChunkedState::Size)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid chunk body LF",
            )),
        }
    }
    fn read_end_cr(rdr: &mut BytesMut) -> Poll<ChunkedState, io::Error> {
        match byte!(rdr) {
            b'\r' => Ok(Async::Ready(ChunkedState::EndLf)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid chunk end CR",
            )),
        }
    }
    fn read_end_lf(rdr: &mut BytesMut) -> Poll<ChunkedState, io::Error> {
        match byte!(rdr) {
            b'\n' => Ok(Async::Ready(ChunkedState::End)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid chunk end LF",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cmp, io};

    use bytes::{Buf, Bytes, BytesMut};
    use http::{Method, Version};
    use tokio_io::{AsyncRead, AsyncWrite};

    use super::*;
    use error::ParseError;
    use h1::InMessage;
    use httpmessage::HttpMessage;
    use request::Request;

    impl InMessage {
        fn message(self) -> Request {
            match self {
                InMessage::Message(msg) => msg,
                InMessage::MessageWithPayload(msg) => msg,
                _ => panic!("error"),
            }
        }
        fn is_payload(&self) -> bool {
            match *self {
                InMessage::MessageWithPayload(_) => true,
                _ => panic!("error"),
            }
        }
    }

    impl PayloadItem {
        fn chunk(self) -> Bytes {
            match self {
                PayloadItem::Chunk(chunk) => chunk,
                _ => panic!("error"),
            }
        }
        fn eof(&self) -> bool {
            match *self {
                PayloadItem::Eof => true,
                _ => false,
            }
        }
    }

    macro_rules! parse_ready {
        ($e:expr) => {{
            match RequestDecoder::default().decode($e) {
                Ok(Some((msg, _))) => msg,
                Ok(_) => unreachable!("Eof during parsing http request"),
                Err(err) => unreachable!("Error during parsing http request: {:?}", err),
            }
        }};
    }

    macro_rules! expect_parse_err {
        ($e:expr) => {{
            match RequestDecoder::default().decode($e) {
                Err(err) => match err {
                    ParseError::Io(_) => unreachable!("Parse error expected"),
                    _ => (),
                },
                _ => unreachable!("Error expected"),
            }
        }};
    }

    struct Buffer {
        buf: Bytes,
        err: Option<io::Error>,
    }

    impl Buffer {
        fn new(data: &'static str) -> Buffer {
            Buffer {
                buf: Bytes::from(data),
                err: None,
            }
        }
    }

    impl AsyncRead for Buffer {}
    impl io::Read for Buffer {
        fn read(&mut self, dst: &mut [u8]) -> Result<usize, io::Error> {
            if self.buf.is_empty() {
                if self.err.is_some() {
                    Err(self.err.take().unwrap())
                } else {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
                }
            } else {
                let size = cmp::min(self.buf.len(), dst.len());
                let b = self.buf.split_to(size);
                dst[..size].copy_from_slice(&b);
                Ok(size)
            }
        }
    }

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl AsyncWrite for Buffer {
        fn shutdown(&mut self) -> Poll<(), io::Error> {
            Ok(Async::Ready(()))
        }
        fn write_buf<B: Buf>(&mut self, _: &mut B) -> Poll<usize, io::Error> {
            Ok(Async::NotReady)
        }
    }

    // #[test]
    // fn test_req_parse_err() {
    //     let mut sys = System::new("test");
    //     let _ = sys.block_on(future::lazy(|| {
    //         let buf = Buffer::new("GET /test HTTP/1\r\n\r\n");
    //         let readbuf = BytesMut::new();

    //         let mut h1 = Dispatcher::new(buf, |req| ok(Response::Ok().finish()));
    //         assert!(h1.poll_io().is_ok());
    //         assert!(h1.poll_io().is_ok());
    //         assert!(h1.flags.contains(Flags::READ_DISCONNECTED));
    //         assert_eq!(h1.tasks.len(), 1);
    //         future::ok::<_, ()>(())
    //     }));
    // }

    #[test]
    fn test_parse() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n\r\n");

        let mut reader = RequestDecoder::default();
        match reader.decode(&mut buf) {
            Ok(Some((req, _))) => {
                assert_eq!(req.version(), Version::HTTP_11);
                assert_eq!(*req.method(), Method::GET);
                assert_eq!(req.path(), "/test");
            }
            Ok(_) | Err(_) => unreachable!("Error during parsing http request"),
        }
    }

    #[test]
    fn test_parse_partial() {
        let mut buf = BytesMut::from("PUT /test HTTP/1");

        let mut reader = RequestDecoder::default();
        assert!(reader.decode(&mut buf).unwrap().is_none());

        buf.extend(b".1\r\n\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::PUT);
        assert_eq!(req.path(), "/test");
    }

    #[test]
    fn test_parse_post() {
        let mut buf = BytesMut::from("POST /test2 HTTP/1.0\r\n\r\n");

        let mut reader = RequestDecoder::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::POST);
        assert_eq!(req.path(), "/test2");
    }

    #[test]
    fn test_parse_body() {
        let mut buf =
            BytesMut::from("GET /test HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody");

        let mut reader = RequestDecoder::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"body"
        );
    }

    #[test]
    fn test_parse_body_crlf() {
        let mut buf =
            BytesMut::from("\r\nGET /test HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody");

        let mut reader = RequestDecoder::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"body"
        );
    }

    #[test]
    fn test_parse_partial_eof() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n");
        let mut reader = RequestDecoder::default();
        assert!(reader.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
    }

    #[test]
    fn test_headers_split_field() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n");

        let mut reader = RequestDecoder::default();
        assert!{ reader.decode(&mut buf).unwrap().is_none() }

        buf.extend(b"t");
        assert!{ reader.decode(&mut buf).unwrap().is_none() }

        buf.extend(b"es");
        assert!{ reader.decode(&mut buf).unwrap().is_none() }

        buf.extend(b"t: value\r\n\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
        assert_eq!(req.headers().get("test").unwrap().as_bytes(), b"value");
    }

    #[test]
    fn test_headers_multi_value() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             Set-Cookie: c1=cookie1\r\n\
             Set-Cookie: c2=cookie2\r\n\r\n",
        );
        let mut reader = RequestDecoder::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();

        let val: Vec<_> = req
            .headers()
            .get_all("Set-Cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(val[0], "c1=cookie1");
        assert_eq!(val[1], "c2=cookie2");
    }

    #[test]
    fn test_conn_default_1_0() {
        let mut buf = BytesMut::from("GET /test HTTP/1.0\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert!(!req.keep_alive());
    }

    #[test]
    fn test_conn_default_1_1() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert!(req.keep_alive());
    }

    #[test]
    fn test_conn_close() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: close\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(!req.keep_alive());
    }

    #[test]
    fn test_conn_close_1_0() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: close\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(!req.keep_alive());
    }

    #[test]
    fn test_conn_keep_alive_1_0() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: keep-alive\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.keep_alive());
    }

    #[test]
    fn test_conn_keep_alive_1_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: keep-alive\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.keep_alive());
    }

    #[test]
    fn test_conn_other_1_0() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: other\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(!req.keep_alive());
    }

    #[test]
    fn test_conn_other_1_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: other\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.keep_alive());
    }

    #[test]
    fn test_conn_upgrade() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             upgrade: websockets\r\n\
             connection: upgrade\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
    }

    #[test]
    fn test_conn_upgrade_connect_method() {
        let mut buf = BytesMut::from(
            "CONNECT /test HTTP/1.1\r\n\
             content-type: text/plain\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
    }

    #[test]
    fn test_request_chunked() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        if let Ok(val) = req.chunked() {
            assert!(val);
        } else {
            unreachable!("Error");
        }

        // type in chunked
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chnked\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        if let Ok(val) = req.chunked() {
            assert!(!val);
        } else {
            unreachable!("Error");
        }
    }

    #[test]
    fn test_headers_content_length_err_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             content-length: line\r\n\r\n",
        );

        expect_parse_err!(&mut buf)
    }

    #[test]
    fn test_headers_content_length_err_2() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             content-length: -1\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_invalid_header() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             test line\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_invalid_name() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             test[]: line\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_bad_status_line() {
        let mut buf = BytesMut::from("getpath \r\n\r\n");
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_upgrade() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: upgrade\r\n\
             upgrade: websocket\r\n\r\n\
             some raw data",
        );
        let mut reader = RequestDecoder::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert!(!req.keep_alive());
        assert!(req.upgrade());
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"some raw data"
        );
    }

    #[test]
    fn test_http_request_parser_utf8() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             x-test: тест\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(
            req.headers().get("x-test").unwrap().as_bytes(),
            "тест".as_bytes()
        );
    }

    #[test]
    fn test_http_request_parser_two_slashes() {
        let mut buf = BytesMut::from("GET //path HTTP/1.1\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert_eq!(req.path(), "//path");
    }

    #[test]
    fn test_http_request_parser_bad_method() {
        let mut buf = BytesMut::from("!12%()+=~$ /get HTTP/1.1\r\n\r\n");

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_parser_bad_version() {
        let mut buf = BytesMut::from("GET //get HT/11\r\n\r\n");

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_chunked_payload() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let mut reader = RequestDecoder::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"data"
        );
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"line"
        );
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_http_request_chunked_payload_and_next_message() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let mut reader = RequestDecoder::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(
            b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n\
              POST /test2 HTTP/1.1\r\n\
              transfer-encoding: chunked\r\n\r\n"
                .iter(),
        );
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"data");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"line");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert!(msg.eof());

        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(req.chunked().unwrap());
        assert_eq!(*req.method(), Method::POST);
        assert!(req.chunked().unwrap());
    }

    #[test]
    fn test_http_request_chunked_payload_chunks() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );

        let mut reader = RequestDecoder::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(b"4\r\n1111\r\n");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"1111");

        buf.extend(b"4\r\ndata\r");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"data");

        buf.extend(b"\n4");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r");
        assert!(pl.decode(&mut buf).unwrap().is_none());
        buf.extend(b"\n");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"li");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"li");

        //trailers
        //buf.feed_data("test: test\r\n");
        //not_ready!(reader.parse(&mut buf, &mut readbuf));

        buf.extend(b"ne\r\n0\r\n");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"ne");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r\n");
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_parse_chunked_payload_chunk_extension() {
        let mut buf = BytesMut::from(
            &"GET /test HTTP/1.1\r\n\
              transfer-encoding: chunked\r\n\r\n"[..],
        );

        let mut reader = RequestDecoder::default();
        let (msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let mut pl = pl.unwrap();
        assert!(msg.chunked().unwrap());

        buf.extend(b"4;test\r\ndata\r\n4\r\nline\r\n0\r\n\r\n"); // test: test\r\n\r\n")
        let chunk = pl.decode(&mut buf).unwrap().unwrap().chunk();
        assert_eq!(chunk, Bytes::from_static(b"data"));
        let chunk = pl.decode(&mut buf).unwrap().unwrap().chunk();
        assert_eq!(chunk, Bytes::from_static(b"line"));
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert!(msg.eof());
    }
}