#![no_main]
use libfuzzer_sys::fuzz_target;
use rpv_cam::video_tx::find_start_code;

fuzz_target!(|data: &[u8]| {
    if data.len() > 0 {
        let from = data[0] as usize % (data.len() + 1);
        let _ = find_start_code(data, from);
    }
});
