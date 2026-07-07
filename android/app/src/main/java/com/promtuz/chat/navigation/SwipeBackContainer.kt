package com.promtuz.chat.navigation

import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.util.VelocityTracker
import androidx.navigationevent.DirectNavigationEventInput
import androidx.navigationevent.NavigationEvent
import androidx.navigationevent.compose.LocalNavigationEventDispatcherOwner
import kotlin.math.abs

private const val COMPLETE_FRACTION = 0.33f // drag past a third of the width to commit the back
private const val FLING_VELOCITY = 1000f    // ...or fling faster than this (px/s) at any distance

/**
 * Telegram-style interactive back: a direction-locked horizontal swipe (from anywhere, not just the
 * edge) that drives nav3's own predictive-pop transition by feeding progress into the same
 * NavigationEventDispatcher the NavDisplay listens on. We reuse all of nav3's scene rendering and
 * lifecycle — only the trigger is ours, sidestepping the flaky system edge gesture.
 *
 * ponytail: full-area swipe with no per-screen veto; add a canSwipeBack gate if a screen ever needs
 * its own horizontal drag (none do today — the app's screens are vertical lists).
 */
@Composable
fun SwipeBackContainer(
    enabled: Boolean,
    modifier: Modifier = Modifier,
    content: @Composable () -> Unit,
) {
    val dispatcher = LocalNavigationEventDispatcherOwner.current?.navigationEventDispatcher
    val input = remember { DirectNavigationEventInput() }

    DisposableEffect(dispatcher) {
        dispatcher?.addInput(input)
        onDispose { dispatcher?.removeInput(input) }
    }

    Box(
        modifier.pointerInput(enabled, dispatcher) {
            if (!enabled || dispatcher == null) return@pointerInput
            val width = size.width.toFloat()
            val slop = viewConfiguration.touchSlop
            awaitEachGesture {
                val down = awaitFirstDown(requireUnconsumed = false)
                val tracker = VelocityTracker()
                var tracking = false
                var settled = false
                while (true) {
                    val change = awaitPointerEvent().changes.firstOrNull() ?: break
                    if (!change.pressed) {
                        if (tracking) {
                            val progress = ((change.position.x - down.position.x) / width).coerceIn(0f, 1f)
                            val vx = tracker.calculateVelocity().x
                            if (progress >= COMPLETE_FRACTION || vx >= FLING_VELOCITY) input.backCompleted()
                            else input.backCancelled()
                            settled = true
                        }
                        break
                    }
                    tracker.addPosition(change.uptimeMillis, change.position)
                    val dx = change.position.x - down.position.x
                    val dy = change.position.y - down.position.y
                    if (!tracking) {
                        // lock in only on a rightward, horizontal-dominant drag; bail on vertical/left
                        if (dx > slop && dx > abs(dy) * 3) {
                            tracking = true
                            input.backStarted(NavigationEvent(NavigationEvent.EDGE_LEFT, 0f, change.position.x, change.position.y))
                        } else if (abs(dy) > slop || dx < -slop) break
                    }
                    if (tracking) {
                        val progress = (dx / width).coerceIn(0f, 1f)
                        input.backProgressed(NavigationEvent(NavigationEvent.EDGE_LEFT, progress, change.position.x, change.position.y))
                        change.consume()
                    }
                }
                if (tracking && !settled) input.backCancelled()
            }
        }
    ) { content() }
}
