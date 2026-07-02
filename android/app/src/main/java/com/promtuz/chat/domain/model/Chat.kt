package com.promtuz.chat.domain.model

/**
 * TODO:
 *  Convert to traditional class and make these value getters so data is at least partially "live"
 */
data class Chat(
    val identity: ByteArray,
    val nickname: String = "Anonymous",
    val lastMessage: LastMessage,
    val type: ChatType = ChatType.Direct
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as Chat

        if (!identity.contentEquals(other.identity)) return false
        if (nickname != other.nickname) return false
        if (lastMessage != other.lastMessage) return false
        if (type != other.type) return false

        return true
    }

    override fun hashCode(): Int {
        var result = identity.contentHashCode()
        result = 31 * result + nickname.hashCode()
        result = 31 * result + lastMessage.hashCode()
        result = 31 * result + type.hashCode()
        return result
    }
}


enum class ChatType {
    Direct,
    Group
}