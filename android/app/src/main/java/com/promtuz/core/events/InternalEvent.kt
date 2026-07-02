package com.promtuz.core.events

// FIXME: shouldn't be importing presentation state in core events
import com.promtuz.chat.presentation.state.ConnectionState

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

// @formatter:off

@Serializable
sealed class IdentityEvent {
    @SerialName("AddMe")
    @Serializable class AddMe(val ipk: ByteArray, val name: String) : IdentityEvent()
}

@Serializable
sealed class MessageEvent {
    @SerialName("Received")
    @Serializable class Received(val id: String, val from: ByteArray, val content: String, val timestamp: Long) : MessageEvent()
    @SerialName("Sent")
    @Serializable class Sent(val id: String, val to: ByteArray, val content: String, val timestamp: Long) : MessageEvent()
    @SerialName("Failed")
    @Serializable class Failed(val id: String, val to: ByteArray, val reason: String) : MessageEvent()
}

object InternalEvents {
    typealias ConnectionEv = ConnectionState
    typealias IdentityEv   = IdentityEvent
    typealias MessageEv    = MessageEvent
}

@Serializable
sealed class InternalEvent {
    @SerialName("Connection")
    @Serializable data class Connection(val state: ConnectionState) : InternalEvent()

    @SerialName("Identity")
    @Serializable data class Identity(val event: IdentityEvent) : InternalEvent()
}

fun interface EventCallback {
    fun onEvent(bytes: ByteArray)
}
