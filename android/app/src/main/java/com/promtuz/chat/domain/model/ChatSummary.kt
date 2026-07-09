package com.promtuz.chat.domain.model

/** One row in the home chat list — a contact plus its latest-message preview. */
data class ChatSummary(
    val peerHex: String,
    val name: String,
    val lastPreview: String?,
    val timestampMs: Long,
)
