package com.promtuz.chat.domain.model

data class ChatSummary(
    val conversationHex: String,
    /** Display name: a group's title, or the peer's contact name for a 1:1. */
    val name: String,
    /** 0 = direct (a 1:1 chat), 1 = group. */
    val kind: Int = 0,
    val peerHex: String? = null,
    val memberCount: Int = 2,
    val lastPreview: String?,
    /** Stable identity for the newest message; status animation resets when it changes. */
    val lastMessageId: String? = null,
    val lastDispatchId: String? = null,
    /** The last message's media kind (see [mediaLabel]); 0 when it's text. */
    val lastMediaKind: Int = 0,
    val timestampMs: Long,
    /** Pairing state: 0 pending, 1 paired, 2 rejected. Groups are always paired. */
    val status: Int = 1,
    /** Why rejected (a DECLINE_* code), when status = 2: 0 group-build, 1 invite-used, 2 declined. */
    val rejectReason: Int? = null,
    val unreadCount: Int = 0,
    val lastOutgoing: Boolean = false,
    val lastDeleted: Boolean = false,
    /** Delivery status of our last message: 0 pending, 1 sent, 2 failed, 3 delivered, 4 read. */
    val lastStatus: Int = 1,
    /** Core sorts by it, so the list doesn't re-sort. */
    val pinned: Boolean = false,
    val muted: Boolean = false,
    val amMember: Boolean = true,
    val canLeave: Boolean = false,
    /** A pre-rules group we founded that others are still in: leaving or deleting would strand it. */
    val ownerIsStuck: Boolean = false,
    /** Our phone makes this group's changes, so deleting it has to hand that over by leaving. */
    val commits: Boolean = false,
    /** Preserve the group's actual name when seeding a newly opened chat header. */
    val rawTitle: String = "",
    /** A message request we have not accepted. */
    val request: Boolean = false,
) {
    val isGroup: Boolean get() = kind == 1
}
