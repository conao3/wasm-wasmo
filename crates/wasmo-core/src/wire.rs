use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct RequestMeta {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn from_meta(meta: RequestMeta, body: Vec<u8>) -> Self {
        let after_scheme = meta
            .url
            .split_once("://")
            .map_or(meta.url.as_str(), |(_, rest)| rest);
        let path_and_query = after_scheme.find('/').map_or("/", |i| &after_scheme[i..]);
        let (path, query) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        let headers = meta
            .headers
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        Self {
            method: meta.method,
            path: path.to_string(),
            query: query.to_string(),
            headers,
            body,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseMeta<'a> {
    pub status: u16,
    pub headers: &'a [(String, String)],
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body,
        }
    }

    pub fn text(status: u16, body: String) -> Self {
        Self::new(status, "text/plain", body.into_bytes())
    }

    pub fn encode_frame(&self) -> Vec<u8> {
        let meta = serde_json::to_vec(&ResponseMeta {
            status: self.status,
            headers: &self.headers,
        })
        .expect("response meta serializes");
        let mut out = Vec::with_capacity(4 + meta.len() + self.body.len());
        out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
        out.extend_from_slice(&meta);
        out.extend_from_slice(&self.body);
        out
    }
}
