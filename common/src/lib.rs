use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct VideoPacket {
    pub frame_id: u64,
    pub payload: Vec<u8>,
    pub timestamp: u64,
}