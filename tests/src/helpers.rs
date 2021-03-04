//! Various test utilities.

use fm::FMBuilder;
use regex::Regex;
use ykshim_client::{sir_body_ret_ty, TirTrace, TypeId};

extern "C" {
    pub fn add6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64;
}
extern "C" {
    pub fn add_some(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64;
}

/// Fuzzy matches the textual TIR for the trace `tt` with the pattern `ptn`.
pub fn assert_tir(ptn: &str, tt: &TirTrace) {
    let ptn_re = Regex::new(r"%.+?\b").unwrap(); // Names are words prefixed with `%`.
    let text_re = Regex::new(r"\$?.+?\b").unwrap(); // Any word optionally prefixed with `$`.
    let matcher = FMBuilder::new(ptn)
        .unwrap()
        .name_matcher(ptn_re, text_re)
        .distinct_name_matching(true)
        .build()
        .unwrap();

    let res = matcher.matches(&format!("{}", tt));
    if let Err(e) = res {
        panic!("{}", e);
    }
}

pub fn neg_assert_tir(ptn: &str, tt: &TirTrace) {
    let ptn_re = Regex::new(r"%.+?\b").unwrap(); // Names are words prefixed with `%`.
    let text_re = Regex::new(r"\$?.+?\b").unwrap(); // Any word optionally prefixed with `$`.
    let matcher = FMBuilder::new(ptn)
        .unwrap()
        .name_matcher(ptn_re, text_re)
        .distinct_name_matching(true)
        .build()
        .unwrap();

    let res = matcher.matches(&format!("{}", tt));
    if let Ok(()) = res {
        panic!("Error: a match was found");
    }
}

/// Types IDs that we need for tests.
#[repr(C)]
pub struct TestTypes {
    pub t_u8: TypeId,
    pub t_i64: TypeId,
    pub t_string: TypeId,
    pub t_tiny_struct: TypeId,
    pub t_tiny_array: TypeId,
    pub t_tiny_tuple: TypeId,
}

impl TestTypes {
    pub fn new() -> TestTypes {
        // We can't know the type ID of any given type, so this works by defining unmangled
        // functions with known return types and then looking them up by name in the SIR.
        #[no_mangle]
        fn i_return_u8() -> u8 {
            0
        }
        #[no_mangle]
        fn i_return_i64() -> i64 {
            0
        }
        #[no_mangle]
        fn i_return_string() -> String {
            String::new()
        }
        struct TinyStruct(u8);
        #[no_mangle]
        fn i_return_tiny_struct() -> TinyStruct {
            TinyStruct(0)
        }
        #[no_mangle]
        fn i_return_tiny_array() -> [u8; 1] {
            [0]
        }
        #[no_mangle]
        fn i_return_tiny_tuple() -> (u8,) {
            (0,)
        }

        TestTypes {
            t_u8: sir_body_ret_ty("i_return_u8"),
            t_i64: sir_body_ret_ty("i_return_i64"),
            t_string: sir_body_ret_ty("i_return_string"),
            t_tiny_struct: sir_body_ret_ty("i_return_tiny_struct"),
            t_tiny_array: sir_body_ret_ty("i_return_tiny_array"),
            t_tiny_tuple: sir_body_ret_ty("i_return_tiny_tuple"),
        }
    }
}
