package com.promtuz.chat.presentation.state

import androidx.annotation.StringRes
import com.promtuz.chat.R
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

// @formatter:off

@Serializable
enum class ConnectionState(@param:StringRes val text: Int) {
    @SerialName("Disconnected") Disconnected(R.string.state_disconnected),
    @SerialName("Idle") Idle(R.string.state_idle),
    @SerialName("Resolving") Resolving(R.string.state_resolving),
    @SerialName("Connecting") Connecting(R.string.state_connecting),
    @SerialName("Handshaking") Handshaking(R.string.state_handshaking),
    @SerialName("Connected") Connected(R.string.state_connected),
    @SerialName("Reconnecting") Reconnecting(R.string.state_reconnecting),
    @SerialName("Failed") Failed(R.string.state_failed),
    @SerialName("NoInternet") NoInternet(R.string.state_nointernet);

    companion object {
        fun fromInt(i: Int): ConnectionState = entries.getOrElse(i) { Idle }
    }
}