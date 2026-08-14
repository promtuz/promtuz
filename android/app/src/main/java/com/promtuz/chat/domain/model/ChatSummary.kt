package com.promtuz.chat.domain.model

/** One row in the home chat list — a conversation plus its latest-message preview. */
data class ChatSummary(
    /** Hex conversation id — the stable key the chat route carries. */
    val conversationHex: String,
    /** Display name: a group's title, or the peer's contact name for a 1:1. */
    val name: String,
    /** 0 = direct (a 1:1 chat), 1 = group. */
    val kind: Int = 0,
    /** The other party, for a direct chat. Null for a group. */
    val peerHex: String? = null,
    /** Active roster size. 2 for a direct chat. */
    val memberCount: Int = 2,
    val lastPreview: String?,
    val timestampMs: Long,
    /** Pairing state: 0 = pending, 1 = paired, 2 = rejected (PAIRING.md). Groups are always paired. */
    val status: Int = 1,
    /** Why rejected (a DECLINE_* code), when status = 2: 0 group-build, 1 invite-used, 2 declined. */
    val rejectReason: Int? = null,
    /** Unread incoming messages — drives the badge; 0 = none. */
    val unreadCount: Int = 0,
    /** Last message is ours (render a "You:" prefix + delivery tick). */
    val lastOutgoing: Boolean = false,
    /** Last message was tombstoned by delete-for-everyone. */
    val lastDeleted: Boolean = false,
    /** Delivery status of our last message: 0 pending,1 sent,2 failed,3 delivered,4 read. */
    val lastStatus: Int = 1,
    /** We are still in this group. False once we left or were removed. */
    val amMember: Boolean = true,
    /** Leaving is offered — a group we are in and haven't stranded. */
    val canLeave: Boolean = false,
    /** Deleting is offered. */
    val canDelete: Boolean = true,
    /**
     * We founded this group and others are still in it, so neither leaving nor
     * deleting is allowed — the group would be left with nobody to manage it.
     */
    val ownerIsStuck: Boolean = false,
) {
    val isGroup: Boolean get() = kind == 1
}
