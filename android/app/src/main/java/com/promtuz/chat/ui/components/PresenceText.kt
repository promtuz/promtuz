package com.promtuz.chat.ui.components

import android.text.format.DateUtils
import androidx.compose.runtime.*
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.promtuz.chat.domain.model.Presence
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive

/** One clock per visible screen; relative presence ages even without a relay event. */
@Composable
fun rememberPresenceTime(): Long {
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    return produceState(System.currentTimeMillis(), lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            while (isActive) {
                value = System.currentTimeMillis()
                delay(DateUtils.MINUTE_IN_MILLIS)
            }
        }
    }.value
}

fun presenceText(presence: Presence?, nowMs: Long): String? {
    fun relative(at: Long): CharSequence = if (nowMs - at < DateUtils.MINUTE_IN_MILLIS) "just now"
        else DateUtils.getRelativeTimeSpanString(at, nowMs, DateUtils.MINUTE_IN_MILLIS)
    return when (presence) {
        Presence.Online -> "online"
        is Presence.Idle -> if (nowMs - presence.sinceMs < DateUtils.MINUTE_IN_MILLIS) "idle just now"
            else "idle since ${relative(presence.sinceMs)}"
        is Presence.LastSeen -> "last seen ${relative(presence.atMs)}"
        else -> null
    }
}
