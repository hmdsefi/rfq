//! The three WASM exports the page calls. The buffers live in thread-locals, so there are no
//! `unsafe` blocks; the only items the `unsafe_code` lint flags are the `#[unsafe(no_mangle)]`
//! export attributes, which `lib.rs` allows for this module alone.
use std::cell::RefCell;

use crate::api::{self, App};

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Sizes the input buffer to `len` bytes; the page writes its JSON request there.
#[unsafe(no_mangle)]
pub extern "C" fn in_buf(len: u32) -> *mut u8 {
    let len = usize::try_from(len).unwrap_or(0);
    INPUT.with(|b| {
        let mut b = b.borrow_mut();
        b.clear();
        b.resize(len, 0);
        b.as_mut_ptr()
    })
}

/// Handles the request in the input buffer. Returns the response length; read it at `out_ptr`.
#[unsafe(no_mangle)]
pub extern "C" fn handle() -> u32 {
    let input = INPUT.with(|b| String::from_utf8(b.borrow().clone()));
    let out = match input {
        Ok(text) => APP.with(|app| api::handle(&mut app.borrow_mut(), &text)),
        Err(_) => r#"{"error":"the request is not UTF-8"}"#.to_string(),
    };
    let len = u32::try_from(out.len()).unwrap_or(0);
    OUTPUT.with(|o| *o.borrow_mut() = out.into_bytes());
    len
}

/// Where the last response starts.
#[unsafe(no_mangle)]
pub extern "C" fn out_ptr() -> *const u8 {
    OUTPUT.with(|o| o.borrow().as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(request: &[u8]) -> String {
        let len = u32::try_from(request.len()).expect("small request");
        in_buf(len);
        INPUT.with(|b| b.borrow_mut().copy_from_slice(request));
        let n = usize::try_from(handle()).expect("fits");
        OUTPUT.with(|o| String::from_utf8(o.borrow()[..n].to_vec()).expect("UTF-8"))
    }

    #[test]
    fn handle_reads_the_input_buffer_and_fills_the_output_buffer() {
        let out = roundtrip(br#"{"op":"state"}"#);
        assert!(out.contains(r#""mode":"sandbox""#), "{out}");
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_trap() {
        assert_eq!(
            roundtrip(&[0xff, 0xfe]),
            r#"{"error":"the request is not UTF-8"}"#
        );
    }
}
