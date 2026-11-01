package com.promtuz.chat.ui.camera

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import android.net.Uri
import java.io.Closeable
import java.io.File
import com.promtuz.chat.utils.extensions.fromHex

/** A finished capture lease; the composer releases it once core has imported it. */
class CapturedMedia(val uri: Uri, private val cleanup: () -> Unit) : Closeable {
    override fun close() = cleanup()
}

class CameraRequest(val conversation: String?, val onCaptured: (CapturedMedia, Boolean) -> Unit) {
    val privateSession = com.promtuz.chat.security.PrivateChatSession.sessionId.takeIf {
        com.promtuz.chat.security.PrivateChatSession.active.value == conversation
    }
    val isPrivate = conversation != null && com.promtuz.core.CoreBridge.privateConversationIds().any {
        it.contentEquals(conversation.fromHex())
    }
}

/** An overlay above navigation, not a route, so the chat keeps its composer and panel as they were. */
object CameraLauncher {
    var request by mutableStateOf<CameraRequest?>(null)
        private set

    fun open(conversation: String? = null, onCaptured: (CapturedMedia, Boolean) -> Unit) {
        request = CameraRequest(conversation, onCaptured)
    }

    fun close() {
        request = null
    }

    /** The attach panel's live tile registers here so the overlay can grow out of it. */
    const val TILE_ORIGIN = "camera-tile"
}
