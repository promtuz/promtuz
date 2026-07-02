package com.promtuz.chat.domain.model

data class LastMessage(
    val content: String?,
    val timestamp: Long,
    val type: MessageType = MessageType.Content,
)