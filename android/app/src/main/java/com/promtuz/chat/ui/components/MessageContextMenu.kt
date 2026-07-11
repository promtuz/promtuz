package com.promtuz.chat.ui.components

import android.view.animation.OvershootInterpolator
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.EaseOutQuint
import androidx.compose.animation.core.Easing
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
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
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.domain.model.UiMessage
import kotlin.math.roundToInt
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/** What was long-pressed: the message, its row bounds in root space, its merge shape. */
data class MenuAnchor(
    val msg: UiMessage,
    val bounds: Rect,
    val mergedTop: Boolean,
    val mergedBottom: Boolean,
)

/** What the finger was over at release. */
sealed interface MenuHit {
    data class Reaction(val emoji: String) : MenuHit
    data class Action(val action: MenuAction) : MenuHit
}

/**
 * Shared state between the pressed bubble — which owns the continuous pointer
 * stream, because the long-press that opens the menu is the same finger that
 * drags to an item — and the overlay, which owns the visuals. Items register
 * live [LayoutCoordinates]; hit-testing queries them at event time, so bounds
 * stay correct even mid-pop-animation.
 */
class MessageMenuState {
    var anchor by mutableStateOf<MenuAnchor?>(null)
        private set
    internal var closing by mutableStateOf(false)

    /** Flat hover index: 0..reactions-1 = strip chips, then action rows. -1 = none. */
    internal var hovered by mutableIntStateOf(-1)

    internal var reactions: List<String> = emptyList()
    internal var actions: List<MenuAction> = emptyList()
    internal val chipCoords = arrayOfNulls<LayoutCoordinates>(MAX_ITEMS)
    internal val rowCoords = arrayOfNulls<LayoutCoordinates>(MAX_ITEMS)

    /** Drag-release on a strip emoji lands here (actions carry their own onClick). */
    var onReact: ((String) -> Unit)? = null

    val isOpen get() = anchor != null

    fun open(anchor: MenuAnchor) {
        if (isOpen) return
        chipCoords.fill(null)
        rowCoords.fill(null)
        hovered = -1
        closing = false
        this.anchor = anchor
    }

    /** Play the exit; the anchor releases when it finishes. */
    fun close() {
        if (isOpen) closing = true
    }

    internal fun closed() {
        anchor = null
        closing = false
        hovered = -1
    }

    /** Track the dragging finger (root coords). Returns true when the hover target changed to an item. */
    fun drag(at: Offset): Boolean {
        if (closing) return false
        val h = hitIndex(at)
        if (h == hovered) return false
        hovered = h
        return h != -1
    }

    /** Finger up: what it landed on (null = nothing). Clears hover. */
    fun release(at: Offset): MenuHit? {
        val h = hitIndex(at)
        hovered = -1
        if (closing) return null
        return when {
            h < 0 -> null
            h < reactions.size -> MenuHit.Reaction(reactions[h])
            else -> actions.getOrNull(h - reactions.size)?.let { MenuHit.Action(it) }
        }
    }

    private fun hitIndex(at: Offset): Int {
        for (i in reactions.indices) {
            val c = chipCoords[i] ?: continue
            if (c.isAttached && c.boundsInRootLive().contains(at)) return i
        }
        for (i in actions.indices) {
            val c = rowCoords[i] ?: continue
            if (c.isAttached && c.boundsInRootLive().contains(at)) return reactions.size + i
        }
        return -1
    }

    private companion object {
        const val MAX_ITEMS = 16
    }
}

/** Query-time bounds (not cached rects) so animated transforms are accounted for. */
private fun LayoutCoordinates.boundsInRootLive(): Rect {
    val tl = positionInRoot()
    return Rect(tl.x, tl.y, tl.x + size.width, tl.y + size.height)
}

private val Overshoot = Easing { OvershootInterpolator(1.1f).getInterpolation(it) }

/**
 * The long-press overlay: the list hides the pressed row and this re-composes the
 * same bubble at its captured bounds, gently lifted (scale → ~1.03) over a 20%
 * scrim — zero reparenting. Reaction strip + action card ([MenuCard], the same
 * surface AppDropMenu uses) pop in anchored to the bubble's side, flipping above
 * when cramped. Everything animates both ways: [MessageMenuState.close] plays the
 * exit and only then releases the anchor. Drag-select rides [MessageMenuState].
 */
@Composable
fun MessageContextMenu(
    state: MessageMenuState,
    quickReactions: List<String>,
    actionGroups: List<List<MenuAction>>,
    onReact: (String) -> Unit,
) {
    val anchor = state.anchor ?: return
    state.reactions = quickReactions
    state.actions = remember(actionGroups) { actionGroups.flatten() }

    val scrim = remember { Animatable(0f) }
    val pop = remember { Animatable(0f) }
    LaunchedEffect(Unit) {
        launch { scrim.animateTo(0.2f, tween(320, easing = EaseOutQuint)) }
        launch { pop.animateTo(1f, tween(250, easing = Overshoot)) }
    }
    LaunchedEffect(state.closing) {
        if (state.closing) {
            launch { scrim.animateTo(0f, tween(160)) }
            pop.animateTo(0f, tween(160))
            state.closed()
        }
    }

    // Root offset makes bounds (captured in window-root space) local to this overlay.
    var origin by remember { mutableStateOf(Offset.Zero) }

    Box(Modifier.fillMaxSize().onGloballyPositioned { origin = it.positionInRoot() }) {
        Box(
            Modifier
                .fillMaxSize()
                .graphicsLayer { alpha = scrim.value }
                .background(Color.Black)
                .pointerInput(Unit) { detectTapGestures { state.close() } },
        )

        // The lifted bubble: pixel-identical at its position, scaling up with the pop.
        Box(
            Modifier
                .offset {
                    IntOffset(
                        (anchor.bounds.left - origin.x).roundToInt(),
                        (anchor.bounds.top - origin.y).roundToInt(),
                    )
                }
                .graphicsLayer {
                    val s = 1f + 0.03f * pop.value
                    scaleX = s
                    scaleY = s
                },
        ) {
            MessageBubble(msg = anchor.msg, mergedTop = anchor.mergedTop, mergedBottom = anchor.mergedBottom)
        }

        MenuStack(state, anchor, quickReactions, actionGroups, pop.value, origin, onReact)
    }
}

/** Measures strip + card, places them around the bubble (below; flips above if cramped). */
@Composable
private fun MenuStack(
    state: MessageMenuState,
    anchor: MenuAnchor,
    quickReactions: List<String>,
    actionGroups: List<List<MenuAction>>,
    pop: Float,
    origin: Offset,
    onReact: (String) -> Unit,
) {
    val outgoing = anchor.msg.outgoing
    val pivot = TransformOrigin(if (outgoing) 1f else 0f, 0.1f)
    val entrance = Modifier.graphicsLayer {
        alpha = pop.coerceIn(0f, 1f)
        scaleX = 0.75f + 0.25f * pop
        scaleY = 0.75f + 0.25f * pop
        transformOrigin = pivot
    }

    Layout(
        content = {
            ReactionStrip(state, anchor.msg, quickReactions, entrance, onReact)
            MenuCard(
                groups = actionGroups,
                hovered = state.hovered - quickReactions.size,
                modifier = entrance,
                onRowPositioned = { i, coords -> state.rowCoords[i] = coords },
                onPick = { it.onClick() },
            )
        },
        modifier = Modifier.fillMaxSize(),
    ) { measurables, constraints ->
        val loose = constraints.copy(minWidth = 0, minHeight = 0)
        val strip = measurables[0].measure(loose)
        val card = measurables[1].measure(loose)

        layout(constraints.maxWidth, constraints.maxHeight) {
            val margin = 14.dp.roundToPx()
            val gap = 8.dp.roundToPx()
            val top = (anchor.bounds.top - origin.y).roundToInt()
            val bottom = (anchor.bounds.bottom - origin.y).roundToInt()
            fun xFor(w: Int) = if (outgoing) constraints.maxWidth - margin - w else margin

            var stripY = top - gap - strip.height
            var cardY = bottom + gap
            if (cardY + card.height > constraints.maxHeight - margin) {
                cardY = top - gap - card.height
                stripY = cardY - gap - strip.height
            }
            stripY = stripY.coerceAtLeast(margin)
            cardY = cardY.coerceAtLeast(margin + strip.height + gap)

            strip.place(xFor(strip.width), stripY)
            card.place(xFor(card.width), cardY)
        }
    }
}

@Composable
private fun ReactionStrip(
    state: MessageMenuState,
    msg: UiMessage,
    emojis: List<String>,
    entrance: Modifier,
    onReact: (String) -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    Row(
        entrance
            .clip(RoundedCornerShape(26.dp))
            .background(colors.surfaceContainerHigh)
            .padding(horizontal = 6.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        emojis.forEachIndexed { i, emoji ->
            val chipPop = remember { Animatable(0f) }
            LaunchedEffect(Unit) {
                delay(40L + 30L * i)
                chipPop.animateTo(1f, tween(220, easing = Overshoot))
            }
            val mine = msg.reactions.any { it.emoji == emoji && it.mine }
            val hoveredHere = state.hovered == i
            Box(
                Modifier
                    .onGloballyPositioned { state.chipCoords[i] = it }
                    .graphicsLayer {
                        val s = chipPop.value * (if (hoveredHere) 1.25f else 1f)
                        alpha = chipPop.value.coerceIn(0f, 1f)
                        scaleX = s
                        scaleY = s
                    }
                    .clip(CircleShape)
                    .background(
                        when {
                            hoveredHere -> colors.surfaceContainerHighest
                            mine -> colors.primary.copy(alpha = 0.22f)
                            else -> Color.Transparent
                        }
                    )
                    .clickable { onReact(emoji) }
                    .padding(horizontal = 7.dp, vertical = 5.dp),
            ) {
                Text(emoji, fontSize = 21.sp)
            }
        }
    }
}
