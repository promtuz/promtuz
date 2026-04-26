package com.promtuz.chat.domain.model

import kotlinx.serialization.Serializable

/// Mirrors Rust MessageRow — CBOR decoded from getMessages()/getConversations()
@Serializable
data class MessageData(
    val id: String,
    val peer_ipk: ByteArray,
    val content: String,
    val outgoing: Boolean,
    val timestamp: Long,
    /// 0 = pending, 1 = sent, 2 = failed
    val status: Int
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false
        other as MessageData
        return id == other.id
    }

    override fun hashCode(): Int = id.hashCode()
}

/// Mirrors Rust ContactInfo — CBOR decoded from getContacts()
@Serializable
data class ContactData(
    val ipk: ByteArray,
    val name: String,
    val added_at: Long
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false
        other as ContactData
        return ipk.contentEquals(other.ipk)
    }

    override fun hashCode(): Int = ipk.contentHashCode()
}
