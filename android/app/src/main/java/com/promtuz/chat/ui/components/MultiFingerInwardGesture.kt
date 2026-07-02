package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.pointerInput
import kotlin.math.hypot

@Composable
fun MultiFingerInwardGesture(
    fingerCount: Int = 4,
    onTrigger: () -> Unit,
    content: @Composable () -> Unit
) {
    Box(
        Modifier
            .fillMaxSize()
            .pointerInput(Unit) {
                awaitPointerEventScope {
                    while (true) {
                        val down = awaitPointerEvent().changes
                            .filter { it.pressed }
                        if (down.size < fingerCount) continue

                        // Track initial positions
                        val start = down.associate { it.id to it.position }

                        // Wait for movement
                        val move = awaitPointerEvent().changes
                        val end = move.associate { it.id to it.position }

                        // Compute center of screen
                        val center = size.run { Offset(width / 2f, height / 2f) }

                        // Check all fingers moved closer to center
                        val allMovedInward = start.keys.all { id ->
                            val s = start[id] ?: return@all false
                            val e = end[id] ?: return@all false
                            e.getDistanceTo(center) < s.getDistanceTo(center)
                        }

                        if (allMovedInward) {
                            onTrigger()
                        }
                    }
                }
            }
    ) {
        content()
    }
}

private fun Offset.getDistanceTo(other: Offset) =
    hypot(x - other.x, y - other.y)
