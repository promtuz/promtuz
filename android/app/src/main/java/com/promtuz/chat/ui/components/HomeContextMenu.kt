package com.promtuz.chat.ui.components

import android.view.animation.OvershootInterpolator
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Easing
import androidx.compose.animation.core.EaseOutQuint
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.unit.dp
import kotlin.math.roundToInt
import kotlinx.coroutines.launch

/** A long-pressed row: where the finger went down (root coords) + the menu to show. */
data class HomeMenuAnchor(val pressRoot: Offset, val groups: List<List<MenuAction>>)

/**
 * Shared state between the pressed row — which owns the continuous pointer stream,
 * since the long-press that opens the menu is the same finger that drags to an item —
 * and the overlay, which owns the visuals. Rows register live [LayoutCoordinates];
 * hit-testing queries them at event time so bounds stay right mid pop-animation.
 */
class HomeMenuState {
    var anchor by mutableStateOf<HomeMenuAnchor?>(null)
        private set
    internal var closing by mutableStateOf(false)
    internal var hovered by mutableIntStateOf(-1)

    private var actions: List<MenuAction> = emptyList()
    internal val rowCoords = arrayOfNulls<LayoutCoordinates>(MAX_ITEMS)

    val isOpen get() = anchor != null

    fun open(anchor: HomeMenuAnchor) {
        if (isOpen) return
        rowCoords.fill(null)
        actions = anchor.groups.flatten()
        hovered = -1
        closing = false
        this.anchor = anchor
    }

    fun close() { if (isOpen) closing = true }

    internal fun closed() {
        anchor = null
        closing = false
        hovered = -1
    }

    /** Track the dragging finger (root coords). True when the hover target became an item. */
    fun drag(at: Offset): Boolean {
        if (closing) return false
        val h = hitIndex(at)
        if (h == hovered) return false
        hovered = h
        return h != -1
    }

    /** Finger up: the action it landed on (null = nothing). Clears hover. */
    fun release(at: Offset): MenuAction? {
        val h = hitIndex(at)
        hovered = -1
        if (closing) return null
        return actions.getOrNull(h)
    }

    private fun hitIndex(at: Offset): Int {
        for (i in actions.indices) {
            val c = rowCoords[i] ?: continue
            if (c.isAttached && c.boundsInRootLive().contains(at)) return i
        }
        return -1
    }

    private companion object {
        const val MAX_ITEMS = 12
    }
}

/** Query-time bounds (not cached rects) so animated transforms are accounted for. */
private fun LayoutCoordinates.boundsInRootLive(): Rect {
    val tl = positionInRoot()
    return Rect(tl.x, tl.y, tl.x + size.width, tl.y + size.height)
}

private val Overshoot = Easing { OvershootInterpolator(1.1f).getInterpolation(it) }

/**
 * The long-press overlay: a scrim plus [MenuCard] (the shared surface) popped in at
 * the finger, clamped to the visible safe area. Drag-select rides [HomeMenuState] —
 * items register live coords, the pressed row feeds the finger. Renders nothing when
 * closed. [close][HomeMenuState.close] plays the exit, then releases the anchor.
 */
@Composable
fun HomeContextMenu(state: HomeMenuState) {
    val anchor = state.anchor ?: return

    val scrim = remember { Animatable(0f) }
    val pop = remember { Animatable(0f) }
    LaunchedEffect(Unit) {
        launch { scrim.animateTo(0.28f, tween(300, easing = EaseOutQuint)) }
        pop.animateTo(1f, tween(220, easing = Overshoot))
    }
    LaunchedEffect(state.closing) {
        if (state.closing) {
            launch { scrim.animateTo(0f, tween(150)) }
            pop.animateTo(0f, tween(150))
            state.closed()
        }
    }

    var origin by remember { mutableStateOf(Offset.Zero) }

    Box(Modifier.fillMaxSize().onGloballyPositioned { origin = it.positionInRoot() }) {
        Box(
            Modifier
                .fillMaxSize()
                .graphicsLayer { alpha = scrim.value }
                .background(Color.Black)
                .pointerInput(Unit) { detectTapGestures { state.close() } },
        )

        // Fit inside the visible safe area, then place the card at the finger — clamped
        // so it never overflows or sinks behind the bars/keyboard.
        Box(
            Modifier
                .fillMaxSize()
                .windowInsetsPadding(WindowInsets.systemBars.union(WindowInsets.ime)),
        ) {
            var stackOrigin by remember { mutableStateOf(Offset.Zero) }
            Box(Modifier.fillMaxSize().onGloballyPositioned { stackOrigin = it.positionInRoot() }) {
                Layout(
                    content = {
                        MenuCard(
                            groups = anchor.groups,
                            hovered = state.hovered,
                            modifier = Modifier.graphicsLayer {
                                val p = pop.value
                                alpha = p.coerceIn(0f, 1f)
                                scaleX = 0.8f + 0.2f * p
                                scaleY = 0.8f + 0.2f * p
                                transformOrigin = TransformOrigin(0f, 0f)
                            },
                            itemHeight = 46.dp,
                            onRowPositioned = { i, c -> state.rowCoords[i] = c },
                            onPick = { it.onClick(); state.close() },
                        )
                    },
                    modifier = Modifier.fillMaxSize(),
                ) { measurables, constraints ->
                    val loose = constraints.copy(minWidth = 0, minHeight = 0)
                    val card = measurables[0].measure(loose)
                    layout(constraints.maxWidth, constraints.maxHeight) {
                        val margin = 8.dp.roundToPx()
                        val px = (anchor.pressRoot.x - stackOrigin.x).roundToInt()
                        val py = (anchor.pressRoot.y - stackOrigin.y).roundToInt()
                        val x = px.coerceIn(margin, (constraints.maxWidth - card.width - margin).coerceAtLeast(margin))
                        val y = py.coerceIn(margin, (constraints.maxHeight - card.height - margin).coerceAtLeast(margin))
                        card.place(x, y)
                    }
                }
            }
        }
    }
}
