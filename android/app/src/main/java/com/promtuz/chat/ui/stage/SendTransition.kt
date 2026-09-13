package com.promtuz.chat.ui.stage

import androidx.compose.animation.core.Animatable
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/** One send owns one clock, started only when acceptance and the measured row are ready. */
class SendTransition internal constructor() {
    val progress = Animatable(0f)
    private var claimed = false
    private var measured = false
    private var started = false
    private var acceptedScope: CoroutineScope? = null
    private var fallback: Job? = null

    fun claim(): SendTransition? = if (claimed || started) null else this.also { claimed = true }

    internal fun measured(): Boolean {
        val fallbackAlreadyStarted = started && !measured
        measured = true
        if (acceptedScope != null) start()
        return !fallbackAlreadyStarted
    }

    internal fun accept(scope: CoroutineScope) {
        acceptedScope = scope
        if (measured) start()
        else fallback = scope.launch {
            // A core call can return before producing a row. Never strand captured
            // text if it produces no row; a later message gets a fresh entrance.
            delay(1000)
            start()
        }
    }

    internal fun reject(scope: CoroutineScope) {
        fallback?.cancel()
        started = true
        scope.launch { progress.snapTo(1f) }
    }

    private fun start() {
        if (started) return
        val scope = acceptedScope ?: return
        started = true
        fallback?.cancel()
        scope.launch { progress.animateTo(1f, ChatMotion.spec()) }
    }
}
