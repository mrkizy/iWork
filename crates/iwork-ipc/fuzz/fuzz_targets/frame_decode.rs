//! 任意字节喂 `decode_frame`，两个方向、握手前后两种上限：不 panic，切出的帧不超过上限。
#![no_main]

use bytes::BytesMut;
use iwork_ipc::frame::{Direction, decode_frame};
use iwork_proto::limits::{MAX_FRAME_LEN, MAX_HELLO_LEN};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for direction in [Direction::ToServer, Direction::ToClient] {
        for max in [MAX_HELLO_LEN, MAX_FRAME_LEN] {
            let mut buf = BytesMut::from(data);
            while let Ok(Some(frame)) = decode_frame(&mut buf, max, direction) {
                assert!(frame.encoded_len() <= max as usize + 4);
            }
        }
    }
});
