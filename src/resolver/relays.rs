use std::sync::Arc;

use common::msg::RelayId;
use quinn::Connection;

use crate::proto::api::RelayDescriptor;

#[derive(Debug)]
pub struct RelayEntry {
  pub id: RelayId,
  pub conn: Arc<Connection>
}

impl RelayEntry {
  pub fn to_descriptor(&self) -> RelayDescriptor {
    todo!()
  }
}