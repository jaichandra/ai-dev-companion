//! The HTTP seam. Every API client takes a transport instead of calling the
//! network itself, the way the Node clients took an injectable `fetchImpl`, so
//! tests drive them with canned replies and nothing touches a socket.

use std::future::Future;
use std::time::Duration;

/// What to do with a 3xx answer: `Follow` like fetch's default (credentials
/// are dropped when the origin changes), `Error` like `redirect: "error"`, or
/// `Manual` like `redirect: "manual"`: the 3xx answer itself is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Redirect {
    #[default]
    Follow,
    Error,
    Manual,
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    pub redirect: Redirect,
}

impl HttpRequest {
    pub fn new(method: &str, url: &str, timeout: Duration) -> Self {
        HttpRequest { method: method.into(), url: url.into(), headers: vec![], body: None, timeout, redirect: Redirect::Follow }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn redirect(mut self, redirect: Redirect) -> Self {
        self.redirect = redirect;
        self
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    /// The body as text (invalid UTF-8 replaced, as `Response.text()` does).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    ConnectionRefused,
    ConnectionReset,
    Timeout,
    /// The server's certificate isn't trusted; the code is the OpenSSL-style
    /// name the Node service reported (`SELF_SIGNED_CERT_IN_CHAIN`, ...).
    Certificate(&'static str),
    Other(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::ConnectionRefused => write!(f, "connection refused"),
            TransportError::ConnectionReset => write!(f, "connection reset"),
            TransportError::Timeout => write!(f, "the request timed out"),
            TransportError::Certificate(code) => write!(f, "the server's certificate isn't trusted ({code})"),
            TransportError::Other(m) => write!(f, "{m}"),
        }
    }
}

pub trait HttpTransport: Send + Sync {
    fn send(&self, req: HttpRequest) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}

/// The real transport: reqwest over rustls, trusting the operating system's
/// root certificates (so an internal CA installed in the Keychain works).
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, String> {
        // Redirects are handled in `send`, so `Redirect::Error` can refuse them.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(ReqwestTransport { client })
    }
}

fn classify(err: &reqwest::Error) -> TransportError {
    if err.is_timeout() {
        return TransportError::Timeout;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => return TransportError::ConnectionRefused,
                std::io::ErrorKind::ConnectionReset => return TransportError::ConnectionReset,
                _ => {}
            }
        }
        let text = e.to_string();
        if text.contains("invalid peer certificate") || text.contains("certificate verify failed") {
            return TransportError::Certificate(certificate_code(&text));
        }
        source = e.source();
    }
    TransportError::Other(err.to_string())
}

/// rustls' words for why a certificate failed, as the OpenSSL-style code the Node service reported.
fn certificate_code(text: &str) -> &'static str {
    if text.contains("UnknownIssuer") {
        "UNABLE_TO_GET_ISSUER_CERT_LOCALLY"
    } else if text.contains("SelfSigned") || text.contains("self-signed") {
        "SELF_SIGNED_CERT_IN_CHAIN"
    } else if text.contains("Expired") {
        "CERT_HAS_EXPIRED"
    } else if text.contains("NotValidForName") {
        "ERR_TLS_CERT_ALTNAME_INVALID"
    } else {
        "UNABLE_TO_VERIFY_LEAF_SIGNATURE"
    }
}

const MAX_REDIRECTS: u32 = 20;

impl HttpTransport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| TransportError::Other(e.to_string()))?;
        let mut url = req.url.clone();
        let mut headers = req.headers.clone();
        let mut body = req.body.clone();
        for hop in 0..=MAX_REDIRECTS {
            let mut builder = self.client.request(method.clone(), &url).timeout(req.timeout);
            for (k, v) in &headers {
                builder = builder.header(k, v);
            }
            if let Some(b) = &body {
                builder = builder.body(b.clone());
            }
            let res = builder.send().await.map_err(|e| classify(&e))?;
            let status = res.status();
            let location = res.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
            if status.is_redirection() && req.redirect != Redirect::Manual {
                if req.redirect == Redirect::Error {
                    return Err(TransportError::Other("unexpected redirect".into()));
                }
                if let Some(location) = location {
                    if hop == MAX_REDIRECTS {
                        return Err(TransportError::Other("too many redirects".into()));
                    }
                    let current = url::Url::parse(&url).map_err(|e| TransportError::Other(e.to_string()))?;
                    let next = current.join(&location).map_err(|e| TransportError::Other(e.to_string()))?;
                    // Credentials never follow a redirect to another origin.
                    if next.origin() != current.origin() {
                        headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("cookie"));
                    }
                    // 301/302/303 turn a non-GET/HEAD request into a bodiless GET.
                    if matches!(status.as_u16(), 301..=303) && method != reqwest::Method::GET && method != reqwest::Method::HEAD {
                        method = reqwest::Method::GET;
                        body = None;
                    }
                    url = next.to_string();
                    continue;
                }
            }
            let out_headers = res
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
                .collect();
            let out_body = res.bytes().await.map_err(|e| classify(&e))?.to_vec();
            return Ok(HttpResponse { status: status.as_u16(), headers: out_headers, body: out_body });
        }
        Err(TransportError::Other("too many redirects".into()))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A canned transport for the client tests.
    use super::*;
    use std::sync::Mutex;

    type Handler = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync>;

    pub struct FakeTransport {
        handler: Handler,
        pub seen: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        pub fn new(handler: impl Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync + 'static) -> Self {
            FakeTransport { handler: Box::new(handler), seen: Mutex::new(vec![]) }
        }

        pub fn replying(status: u16, body: &str, content_type: &str) -> Self {
            let (body, ct) = (body.to_string(), content_type.to_string());
            Self::new(move |_| {
                Ok(HttpResponse {
                    status,
                    headers: vec![("content-type".into(), ct.clone())],
                    body: body.clone().into_bytes(),
                })
            })
        }
    }

    impl HttpTransport for FakeTransport {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
            let out = (self.handler)(&req);
            self.seen.lock().unwrap().push(req);
            out
        }
    }
}
