package com.promtuz.chat.domain.model

/** One row in the home chat list — a contact plus its latest-message preview. */
data class ChatSummary(
    val peerHex: String,
    val name: String,
    val lastPreview: String?,
    val timestampMs: Long,
    /** Pairing state: 0 = pending, 1 = paired, 2 = rejected (PAIRING.md). */
    val status: Int = 1,
)
