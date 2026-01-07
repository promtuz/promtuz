use quinn::{Connection, VarInt};

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum CloseReason {
    DuplicateConnect,
    AlreadyConnected,
    ShuttingDown,
    Reconnecting,
    PacketMismatch
}

impl CloseReason {
    pub fn reason(&self) -> Vec<u8> {
        format!("{:?}", self).into()
    }
    pub fn code(&self) -> VarInt {
        VarInt::from_u32(*self as u32 + 1)
    }

    pub fn close(self, conn: &Connection) {
        conn.close(self.code(), &self.reason());
    }
}
