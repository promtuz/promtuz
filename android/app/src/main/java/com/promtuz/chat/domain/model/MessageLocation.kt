package com.promtuz.chat.domain.model

/** The stored message behind a media item, independent of the screen that opened it. */
data class MessageLocation(
    val conversation: String,
    val chatName: String,
    val dispatchId: String,
    val timestampMs: Long,
)
