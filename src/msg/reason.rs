use quinn::VarInt;

#[derive(Debug)]
pub enum CloseReason {
    DuplicateConnect,
    AlreadyConnected,
    ShuttingDown,
}

impl CloseReason {
    pub fn reason(&self) -> Vec<u8> {
        format!("{:?}", self).into()
    }
    pub fn code(&self) -> VarInt {
        match self {
            CloseReason::DuplicateConnect => VarInt::from_u32(0x01),
            CloseReason::AlreadyConnected => VarInt::from_u32(0x02),
            CloseReason::ShuttingDown => VarInt::from_u32(0x03),
        }
    }
}
