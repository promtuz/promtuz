package com.promtuz.chat.domain.model

data class MessageLocation(
    val conversation: String,
    val chatName: String,
    val dispatchId: String,
    val timestampMs: Long,
)
