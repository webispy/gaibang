//! One GET or POST, no redirects followed (the crawler checks every hop), body cut at a size limit.

use std::io::Read;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct Response {
    /// 0: no answer (DNS, connect, timeout, reset before the status line).
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The body was longer than the limit and was cut.
    pub truncated: bool,
    /// The connection broke while the body was read: the status is real, the body is not whole.
    pub incomplete: bool,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub trait Transport {
    fn get(&self, url: &str, headers: &[(String, String)], max_bytes: usize) -> Response;

    /// A POST (an API's token issuance); transports that never post answer nothing.
    fn post(&self, _url: &str, _headers: &[(String, String)], _body: &[u8], _max_bytes: usize) -> Response {
        Response::default()
    }
}

pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout: Duration) -> UreqTransport {
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build();
        UreqTransport { agent: config.into() }
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str, headers: &[(String, String)], max_bytes: usize) -> Response {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        read(request.call(), max_bytes)
    }

    fn post(&self, url: &str, headers: &[(String, String)], body: &[u8], max_bytes: usize) -> Response {
        let mut request = self.agent.post(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        read(request.send(body), max_bytes)
    }
}

fn read(answer: Result<ureq::http::Response<ureq::Body>, ureq::Error>, max_bytes: usize) -> Response {
    let Ok(mut response) = answer else { return Response::default() };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let mut body = Vec::new();
    let read = response.body_mut().as_reader().take(max_bytes as u64 + 1).read_to_end(&mut body);
    let truncated = body.len() > max_bytes;
    body.truncate(max_bytes);
    Response { status, headers, body, truncated, incomplete: read.is_err() }
}
