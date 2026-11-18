package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onPlaced
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlin.math.abs

/**
 * The selection slab shared by a row (or column) of options. Pressing an option pulls the slab under
 * the finger, dragging carries it along, and lifting confirms; a cancelled gesture springs it back.
 */
@Stable
class SlabState internal constructor(internal val vertical: Boolean) {
    internal var host: LayoutCoordinates? = null
    internal val rects = mutableStateMapOf<Int, Rect>()
    var pressed by mutableStateOf<Int?>(null)
        internal set
    internal val edges = Array(4) { Animatable(0f) }
    internal val lift = Animatable(0f)
    internal var placed by mutableStateOf(false)

    /** The option under [p], or the nearest one along the axis once a drag has left them all. */
    internal fun at(p: Offset, nearest: Boolean): Int? {
        rects.entries.firstOrNull { it.value.contains(p) }?.let { return it.key }
        if (!nearest) return null
        return rects.minByOrNull { (_, r) -> if (vertical) abs(r.center.y - p.y) else abs(r.center.x - p.x) }?.key
    }

    /** The option the slab is heading for: the pressed one, else [selected]. */
    fun shown(selected: Int) = pressed ?: selected
}

@Composable
fun rememberSlabState(vertical: Boolean = false) = remember(vertical) { SlabState(vertical) }

/** On the container: draws the slab behind the options, moves it, and turns presses into selection. */
@Composable
fun Modifier.slabHost(state: SlabState, selected: Int, color: Color, radius: Dp, onSelect: (Int) -> Unit): Modifier {
    val select by rememberUpdatedState(onSelect)
    SlabMotion(state, selected)
    return this.then(Modifier.onPlaced { state.host = it })
        .then(Modifier.pointerInput(state) { slabGestures(state) { select(it) } })
        .then(Modifier.drawBehind {
            if (!state.placed) return@drawBehind
            val e = state.edges
            val lift = state.lift.value
            val grow = 2.dp.toPx() * lift
            val left = e[0].value - grow; val top = e[1].value - grow
            val r = radius.toPx() + grow
            val size = Size(e[2].value - e[0].value + grow * 2, e[3].value - e[1].value + grow * 2)
            drawRoundRect(color, Offset(left, top), size, CornerRadius(r))
            if (lift > 0f) drawRoundRect(Color.White.copy(alpha = 0.06f * lift), Offset(left, top), size, CornerRadius(r))
        })
}

@Composable
private fun SlabMotion(state: SlabState, selected: Int) {
    val target = state.shown(selected)
    val rect = state.rects[target]
    LaunchedEffect(rect) {
        val to = rect ?: return@LaunchedEffect
        val e = state.edges
        val goal = floatArrayOf(to.left, to.top, to.right, to.bottom)
        if (!state.placed) {
            goal.forEachIndexed { i, v -> e[i].snapTo(v) }
            state.placed = true
            return@LaunchedEffect
        }
        // The leading edge springs ahead and the trailing edge follows, so the slab stretches as it travels.
        val forward = if (state.vertical) to.top > e[1].value else to.left > e[0].value
        val lead = spring<Float>(dampingRatio = 0.78f, stiffness = 900f)
        val trail = spring<Float>(dampingRatio = 0.72f, stiffness = 340f)
        val (a, b) = if (state.vertical) 1 to 3 else 0 to 2
        coroutineScope {
            goal.forEachIndexed { i, v ->
                val spec = when (i) {
                    a -> if (forward) trail else lead
                    b -> if (forward) lead else trail
                    else -> lead
                }
                launch { e[i].animateTo(v, spec) }
            }
        }
    }
    val pressed = state.pressed != null
    LaunchedEffect(pressed) {
        state.lift.animateTo(if (pressed) 1f else 0f, spring(dampingRatio = 0.6f, stiffness = Spring.StiffnessMedium))
    }
}

/** On each option: reports its bounds to the slab and carries the tab semantics. */
fun Modifier.slabOption(state: SlabState, index: Int, selected: Boolean, role: Role, onSelect: () -> Unit): Modifier =
    onGloballyPositioned { c ->
        val host = state.host?.takeIf { it.isAttached } ?: return@onGloballyPositioned
        val r = host.localBoundingBoxOf(c, clipBounds = false)
        if (state.rects[index] != r) state.rects[index] = r
    }.semantics(mergeDescendants = true) {
        this.role = role
        this.selected = selected
        onClick { onSelect(); true }
    }

private suspend fun androidx.compose.ui.input.pointer.PointerInputScope.slabGestures(state: SlabState, onSelect: (Int) -> Unit) {
    awaitEachGesture {
        val down = awaitFirstDown(requireUnconsumed = false)
        state.pressed = state.at(down.position, nearest = false) ?: return@awaitEachGesture
        val slop = viewConfiguration.touchSlop
        var dragging = false
        try {
            while (true) {
                val ch = awaitPointerEvent().changes.firstOrNull { it.id == down.id } ?: break
                if (!ch.pressed) {
                    state.pressed?.let(onSelect)
                    break
                }
                if (ch.isConsumed) break
                val d = ch.position - down.position
                val along = if (state.vertical) d.y else d.x
                val across = if (state.vertical) d.x else d.y
                if (!dragging) {
                    // Movement across the row belongs to whatever scrolls around it.
                    if (abs(across) > slop && abs(across) > abs(along)) break
                    if (abs(along) > slop) dragging = true
                }
                if (dragging) {
                    state.at(ch.position, nearest = true)?.let { state.pressed = it }
                    ch.consume()
                }
            }
        } finally {
            state.pressed = null
        }
    }
}
