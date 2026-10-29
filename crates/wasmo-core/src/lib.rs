pub mod codec;
pub mod env;
pub mod model;
pub mod query;
pub mod route;
pub mod services;
pub mod wire;
pub mod xml;

use env::Env;
use route::Route;
use wire::{Request, Response};

pub struct Emulator<E: Env> {
    env: E,
    request_seq: u64,
}

impl<E: Env> Emulator<E> {
    pub fn new(env: E) -> Self {
        Self {
            env,
            request_seq: 0,
        }
    }

    pub fn handle(&mut self, req: &Request) -> Response {
        self.request_seq += 1;
        let request_id = self.request_id();
        let route = Route::from_request(req);
        let mut res = match route.service.as_deref() {
            Some("sts") => services::sts::handle(&route, req),
            Some(other) => {
                Response::text(400, format!("Service '{other}' is not emulated by wasmo"))
            }
            None => Response::text(400, "Unable to determine the target service".to_string()),
        };
        res.headers
            .push(("x-amzn-requestid".to_string(), request_id));
        res
    }

    fn request_id(&mut self) -> String {
        let mut b = [0u8; 16];
        self.env.random_fill(&mut b);
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }
}
