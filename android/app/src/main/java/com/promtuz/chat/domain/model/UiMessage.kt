package com.promtuz.chat.domain.model

import androidx.compose.runtime.Immutable

/** [key] is the shared dispatch id, while [localId] preserves local sort order. */
@Immutable
data class UiMessage(
    val key: String,
    val localId: String,
    val dispatchIdHex: String,
    val content: MessageContent,
    val outgoing: Boolean,
    /** Set only on incoming group messages. */
    val senderHex: String? = null,
    /** Display name for [senderHex], null when unknown. */
    val senderName: String? = null,
    val status: SendStatus,
    val edited: Boolean,
    val deleted: Boolean,
    val timestampMs: Long,
    val reactions: List<ReactionGroup>,
    val quote: Quote? = null,
)

/** [text] is null when the quoted message isn't loaded or was hard-deleted. */
@Immutable
data class Quote(
    val dispatchIdHex: String,
    val text: String?,
)
