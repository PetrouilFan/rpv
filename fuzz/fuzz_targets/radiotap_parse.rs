#![no_main]
use libfuzzer_sys::fuzz_target;
use rpv_proto::rawsock_common::{parse_radiotap_rssi, strip_radiotap};

fuzz_target!(|data: &[u8]| {
    let _ = parse_radiotap_rssi(data);
    let _ = strip_radiotap(data);
});
