package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.appearance.LocalChatColors
import kotlin.math.roundToInt
import kotlinx.coroutines.launch

/**
 * Drag a message left to stage a reply: hard linear clamp at 80dp (no banding),
 * commit at 50dp on release, exactly one haptic at the threshold (guard resets when
 * dragged back under). The cue arrow behind the row fades/scales with progress.
 */
@Composable
fun SwipeToReply(
    enabled: Boolean,
    onReply: () -> Unit,
    modifier: Modifier = Modifier,
    content: @Composable () -> Unit,
) {
    val offsetX = remember { Animatable(0f) }
    // pointerInput keys on `enabled`, which almost never changes, so the gesture
    // coroutine outlives the composition that started it and would keep calling
    // whichever `onReply` it closed over first — replying to a message as it was
    // several edits ago.
    val reply by rememberUpdatedState(onReply)
    val haptic = LocalHapticFeedback.current
    val scope = rememberCoroutineScope()
    val density = LocalDensity.current
    val clampPx = with(density) { 80.dp.toPx() }
    val commitPx = with(density) { 50.dp.toPx() }
    val accent = LocalChatColors.current.accent

    Box(modifier.fillMaxWidth()) {
        DrawableIcon(
            R.drawable.i_reply,
            Modifier
                .align(Alignment.CenterEnd)
                .padding(end = 18.dp)
                .size(22.dp)
                .graphicsLayer {
                    val p = (-offsetX.value / commitPx).coerceIn(0f, 1f)
                    alpha = p
                    scaleX = 0.6f + 0.4f * p
                    scaleY = 0.6f + 0.4f * p
                },
            tint = accent,
        )
        Box(
            Modifier
                .offset { IntOffset(offsetX.value.roundToInt(), 0) }
                .pointerInput(enabled) {
                    if (!enabled) return@pointerInput
                    // Two-phase: observe (never consume) until the direction is
                    // DECISIVELY a left swipe — 3:1 horizontal dominance past slop.
                    // Anything vertical-ish or rightward bails silently, so the
                    // list's scroll never loses a frame to this gesture.
                    awaitEachGesture {
                        val down = awaitFirstDown(requireUnconsumed = false)
                        var dx = 0f
                        var dy = 0f
                        val slop = viewConfiguration.touchSlop
                        while (true) {
                            val ch = awaitPointerEvent().changes.firstOrNull { it.id == down.id }
                                ?: return@awaitEachGesture
                            if (!ch.pressed || ch.isConsumed) return@awaitEachGesture
                            val d = ch.positionChange()
                            dx += d.x
                            dy += d.y
                            if (kotlin.math.abs(dy) > slop && kotlin.math.abs(dy) >= kotlin.math.abs(dx)) {
                                return@awaitEachGesture // the scroll's gesture
                            }
                            if (dx > slop) return@awaitEachGesture // right swipe = nothing
                            if (dx < -slop && kotlin.math.abs(dx) > 3 * kotlin.math.abs(dy)) break
                        }

                        // Claimed: drive the clamp, one haptic at the threshold.
                        //
                        // The offset is tracked here rather than read back off the
                        // Animatable: snapTo is dispatched through `scope.launch`, so
                        // its value trails the finger by however long that takes to
                        // run — and a quick flick ends the gesture with the Animatable
                        // still near zero, which is a commit that never fires. The
                        // Animatable stays the render source; this is the truth.
                        var vibrated = false
                        var offset = dx.coerceIn(-clampPx, 0f)
                        scope.launch { offsetX.snapTo(offset) }
                        while (true) {
                            val ch = awaitPointerEvent().changes.firstOrNull { it.id == down.id } ?: break
                            if (!ch.pressed) break
                            // Read the delta BEFORE consuming: positionChange()
                            // reports zero for a change already marked consumed, so
                            // consuming first silently pins the drag at its claim
                            // distance and the commit threshold is never reached.
                            val step = ch.positionChange().x
                            ch.consume()
                            offset = (offset + step).coerceIn(-clampPx, 0f)
                            scope.launch { offsetX.snapTo(offset) }
                            if (offset <= -commitPx) {
                                if (!vibrated) {
                                    vibrated = true
                                    haptic.performHapticFeedback(HapticFeedbackType.GestureThresholdActivate)
                                }
                            } else vibrated = false
                        }
                        if (offset <= -commitPx) reply()
                        scope.launch {
                            offsetX.animateTo(0f, spring(stiffness = Spring.StiffnessMediumLow))
                        }
                    }
                },
        ) { content() }
    }
}
