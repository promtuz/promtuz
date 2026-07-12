package com.promtuz.chat.domain.model

/** One row in the home chat list — a contact plus its latest-message preview. */
data class ChatSummary(
    val peerHex: String,
    val name: String,
    val lastPreview: String?,
    val timestampMs: Long,
    /** Pairing state: 0 = pending, 1 = paired, 2 = rejected (PAIRING.md). */
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
)
