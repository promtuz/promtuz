package com.promtuz.chat.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import kotlinx.coroutines.withTimeoutOrNull

/**
 * A quiet tap: no ripple, nothing consumed, and only a short press counts. A hold
 * that reaches the long-press timeout belongs to whoever is listening for it (the
 * bubble menu), a drag belongs to the list, and an up someone already consumed is
 * theirs. Media and other borderless surfaces use this so a tap opens and nothing
 * else lights up.
 */
fun Modifier.tapOnly(onTap: () -> Unit): Modifier = this
    .semantics { role = Role.Button; onClick { onTap(); true } }
    .pointerInput(onTap) {
        awaitEachGesture {
            val down = awaitFirstDown(requireUnconsumed = false)
            val up = withTimeoutOrNull(viewConfiguration.longPressTimeoutMillis) {
                waitForUpOrCancellation()
            } ?: return@awaitEachGesture
            if (up == null || up.isConsumed) return@awaitEachGesture
            if ((up.position - down.position).getDistance() > viewConfiguration.touchSlop) return@awaitEachGesture
            onTap()
        }
    }

/**
 * A small control that answers a press by shrinking a little, then acts on release.
 * Its own hit area, its own feedback; the surface it sits on stays still.
 */
fun Modifier.pressScale(onClick: () -> Unit, scaleTo: Float = 0.88f): Modifier = composed {
    val currentOnClick by rememberUpdatedState(onClick)
    var pressed by remember { mutableStateOf(false) }
    val scale by animateFloatAsState(if (pressed) scaleTo else 1f, spring(stiffness = 900f), label = "press")
    this
        .graphicsLayer { scaleX = scale; scaleY = scale }
        .semantics { role = Role.Button; onClick { currentOnClick(); true } }
        .pointerInput(Unit) {
            awaitEachGesture {
                val down = awaitFirstDown()
                down.consume()
                pressed = true
                val up = try { waitForUpOrCancellation() } finally { pressed = false }
                if (up != null) {
                    up.consume()
                    currentOnClick()
                }
            }
        }
}
