#![no_main]
use libfuzzer_sys::fuzz_target;
use rpv_proto::link::L2Header;

fuzz_target!(|data: &[u8]| {
    let _ = L2Header::decode(data);
});
