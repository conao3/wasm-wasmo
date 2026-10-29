use std::cell::RefCell;

use wasmo_core::Emulator;
use wasmo_core::env::Env;
use wasmo_core::wire::{Request, RequestMeta, Response};

#[link(wasm_import_module = "wasmo")]
unsafe extern "C" {
    #[link_name = "now_ms"]
    fn host_now_ms() -> f64;
    #[link_name = "random_fill"]
    fn host_random_fill(ptr: *mut u8, len: usize);
}

struct HostEnv;

impl Env for HostEnv {
    fn now_ms(&self) -> f64 {
        unsafe { host_now_ms() }
    }

    fn random_fill(&mut self, buf: &mut [u8]) {
        unsafe { host_random_fill(buf.as_mut_ptr(), buf.len()) }
    }
}

thread_local! {
    static EMULATOR: RefCell<Emulator<HostEnv>> = RefCell::new(Emulator::new(HostEnv));
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmo_alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// # Safety
///
/// `ptr` and `len` must come from `wasmo_alloc` or from a `wasmo_handle` result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasmo_free(ptr: *mut u8, len: usize) {
    unsafe { drop(Vec::from_raw_parts(ptr, 0, len)) }
}

/// Takes ownership of both input buffers (allocated with `wasmo_alloc`) and
/// returns `(ptr << 32) | len` of a response frame: a big-endian u32 meta
/// length, the meta JSON, then the body bytes. The host frees it with
/// `wasmo_free(ptr, len)`.
///
/// # Safety
///
/// Both buffers must come from `wasmo_alloc` with exactly these lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasmo_handle(
    meta_ptr: *mut u8,
    meta_len: usize,
    body_ptr: *mut u8,
    body_len: usize,
) -> u64 {
    let meta_bytes = unsafe { Vec::from_raw_parts(meta_ptr, meta_len, meta_len) };
    let body = unsafe { Vec::from_raw_parts(body_ptr, body_len, body_len) };
    let res = match serde_json::from_slice::<RequestMeta>(&meta_bytes) {
        Ok(meta) => {
            let req = Request::from_meta(meta, body);
            EMULATOR.with_borrow_mut(|emu| emu.handle(&req))
        }
        Err(e) => Response::text(400, format!("wasmo: invalid request meta: {e}")),
    };
    let mut frame = res.encode_frame();
    frame.shrink_to_fit();
    let len = frame.len();
    let ptr = frame.as_mut_ptr();
    std::mem::forget(frame);
    ((ptr as u64) << 32) | len as u64
}
