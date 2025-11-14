use std::sync::Arc;

use common::msg::RelayId;
use quinn::Connection;

#[derive(Debug)]
pub struct RelayEntry {
  pub id: RelayId,
  pub conn: Arc<Connection>
}
