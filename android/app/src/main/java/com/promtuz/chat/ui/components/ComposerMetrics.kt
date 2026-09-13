package com.promtuz.chat.ui.components

import androidx.compose.runtime.*

/** Actual allocated geometry, published by the composer layout in its measure pass. */
@Stable
class ComposerMetrics {
    internal var sendTransition: com.promtuz.chat.ui.stage.SendTransition? = null
    var composerPx by mutableIntStateOf(0)
        internal set
    var regionPx by mutableIntStateOf(0)
        internal set
    var accessoryPx by mutableIntStateOf(0)
        internal set
    val pushPx: Float get() = accessoryPx.toFloat()
    val bottomPx: Float get() = (composerPx + regionPx).toFloat()
}

@Composable
fun rememberComposerMetrics(): ComposerMetrics = remember { ComposerMetrics() }
