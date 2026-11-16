#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| another_fuzz::tool_args(data));
