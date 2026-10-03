package com.promtuz.chat.ui.camera

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import java.io.File

/** [onCaptured] gets the written file and whether it is a clip. */
class CameraRequest(val onCaptured: (File, Boolean) -> Unit)

/** An overlay above navigation, not a route, so the chat keeps its composer and panel as they were. */
object CameraLauncher {
    var request by mutableStateOf<CameraRequest?>(null)
        private set

    fun open(onCaptured: (File, Boolean) -> Unit) {
        request = CameraRequest(onCaptured)
    }

    fun close() {
        request = null
    }

    /** The attach panel's live tile registers here so the overlay can grow out of it. */
    const val TILE_ORIGIN = "camera-tile"
}
