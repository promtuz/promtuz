use std::sync::Arc;

use common::proto::RelayId;
use common::proto::client_res::RelayDescriptor;
use quinn::Connection;

#[derive(Debug)]
pub struct RelayEntry {
    pub id: RelayId,
    pub conn: Arc<Connection>,
}

impl RelayEntry {
    pub fn to_descriptor(&self) -> RelayDescriptor {
        RelayDescriptor { id: self.id, addr: self.conn.remote_address() }
    }
}
