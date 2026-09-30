//! Shared routing for avatar and profile-details reconciliation.

use std::collections::HashMap;

use anyhow::Result;
use common::proto::mls_wire::AppPayload;

use crate::data::conversation::Conversation;
use crate::data::conversation::KIND_DIRECT;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;

/// Prefer a direct pair, then other usable shared groups. A stale SQL roster
/// is not proof that we can still sign or encrypt in its MLS group.
pub(crate) fn routes(owner: &[u8; 32]) -> HashMap<[u8; 32], Vec<[u8; 16]>> {
    let provider = PromtuzMlsProvider::shared();
    let mut chats = Conversation::list();
    chats.sort_by_key(|c| c.kind != KIND_DIRECT);
    let mut routes: HashMap<_, Vec<_>> = HashMap::new();
    for chat in chats {
        if crate::requests::is_request_chat(&chat.id)
            || crate::groups::is_leaving(&chat.id)
            || crate::groups::delete_pending(&chat.id)
        {
            continue;
        }
        let Some(gid) = Conversation::group_of(&chat.id) else { continue };
        let Ok(Some(group)) = MlsGroupHandle::load(&provider, &gid) else { continue };
        if crate::messaging::leaf_signer_for_group(&provider, &group, owner).is_err() {
            continue;
        }
        for member in group.roster().into_iter().filter(|m| m != owner) {
            routes.entry(member).or_default().push(chat.id);
        }
    }
    routes
}

pub(crate) async fn probe(peer: [u8; 32], routes: &[[u8; 16]], payload: AppPayload) -> Result<()> {
    let mut error = None;
    for route in routes {
        match crate::messaging::send_control_to(*route, payload.clone(), peer).await {
            Ok(()) => return Ok(()),
            // Probes are intentionally ephemeral. An offline/unreachable home
            // is deferred to the next reconciliation, not a failed profile edit.
            Err(e) if e.downcast_ref::<crate::messaging::ControlDeferred>().is_some() => {},
            Err(e) => error = Some(e),
        }
    }
    error.map_or(Ok(()), Err)
}
