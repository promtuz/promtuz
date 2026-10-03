package com.promtuz.chat.presentation.state

import androidx.annotation.StringRes
import com.promtuz.chat.R

/** [Idle] until core reports a state. */
enum class ConnectionState(@param:StringRes val text: Int) {
    Disconnected(R.string.state_disconnected),
    Idle(R.string.state_idle),
    Resolving(R.string.state_resolving),
    Connecting(R.string.state_connecting),
    Handshaking(R.string.state_handshaking),
    Connected(R.string.state_connected),
    Failed(R.string.state_failed),
    Syncing(R.string.state_syncing),
}
