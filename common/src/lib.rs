use serde::{Serialize, Deserialize};
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Debug)]
pub struct VideoPacket {
    pub send_time: SystemTime,
    pub frame_id: u64,
    pub payload: Vec<u8>,
}