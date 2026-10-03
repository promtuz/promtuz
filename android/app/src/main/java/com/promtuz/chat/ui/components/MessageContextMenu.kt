package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableFloatState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
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
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.domain.model.UiMessage
import kotlin.math.roundToInt
import kotlinx.coroutines.delay
import kotlin.time.Duration.Companion.milliseconds
import com.promtuz.chat.ui.text.EmojiText

data class MenuAnchor(
    val msg: UiMessage,
    val bounds: Rect,
    val mergedTop: Boolean,
    val mergedBottom: Boolean,
)

/** Strip reactions and dragged-to reactions both toggle through [onReact] on the anchored message. */
class MessageMenuState(private val onReact: (UiMessage, String) -> Unit) : DragMenuState<MenuAnchor>(32) {
    internal var reactions: List<String> = emptyList()
    internal var actions: List<MenuAction> = emptyList()

    /** How lifted the copy is, 0..1. The list row fades by the same amount, so the two cross-fade. */
    val lift get() = pop.value.coerceIn(0f, 1f)

    fun react(emoji: String) {
        anchor?.let { onReact(it.msg, emoji) }
        close()
    }

    /** Targets are the strip's chips, then the action rows; actions close the menu themselves. */
    internal fun pick(index: Int) {
        val action = actions.getOrNull(index - reactions.size)
        when {
            index in reactions.indices -> react(reactions[index])
            action != null -> action.onClick()
            else -> close()
        }
    }
}

/** Redraws the pressed bubble at its captured bounds while the list hides the original. */
@Composable
fun MessageContextMenu(
    state: MessageMenuState,
    quickReactions: List<String>,
    actionGroups: List<List<MenuAction>>,
    iconSize: Dp = MenuIconSize,
    anchorOffsetY: () -> Float = { 0f },
) {
    val anchor = state.anchor ?: return
    state.reactions = quickReactions
    state.actions = remember(actionGroups) { actionGroups.flatten() }

    // Root offset makes bounds (captured in window-root space) local to this overlay.
    var origin by remember { mutableStateOf(Offset.Zero) }
    // Set by MenuStack's layout when the ensemble needs the bubble out of the way.
    val shift = remember { mutableFloatStateOf(0f) }
    val density = LocalDensity.current

    Box(Modifier.fillMaxSize().onGloballyPositioned { origin = it.positionInRoot() }) {
        MenuBackdrop(state, dim = 0.2f, scrimMs = 320, popMs = 250, exitMs = 160)

        // Laid out at the captured row width so it wraps exactly as it did in the list.
        Box(
            Modifier
                .offset {
                    IntOffset(
                        (anchor.bounds.left - origin.x).roundToInt(),
                        (anchor.bounds.top - origin.y).roundToInt(),
                    )
                }
                .width(with(density) { anchor.bounds.width.toDp() })
                .graphicsLayer {
                    val p = state.pop.value
                    alpha = p.coerceIn(0f, 1f)
                    val s = 1f + 0.03f * p
                    scaleX = s
                    scaleY = s
                    translationY = shift.floatValue * p + anchorOffsetY()
                    transformOrigin = TransformOrigin(if (anchor.msg.outgoing) 1f else 0f, 1f)
                },
        ) {
            MessageBubble(msg = anchor.msg, mergedTop = anchor.mergedTop, mergedBottom = anchor.mergedBottom)
        }

        // The strip and card stay inside the bars and IME, with their own origin for the
        // anchor math; the bubble shifts rather than the card sinking behind the keyboard.
        var stackOrigin by remember { mutableStateOf(Offset.Zero) }
        Box(
            Modifier
                .fillMaxSize()
                .windowInsetsPadding(WindowInsets.systemBars.union(WindowInsets.ime))
                .onGloballyPositioned { stackOrigin = it.positionInRoot() },
        ) {
            MenuStack(state, anchor, quickReactions, actionGroups, iconSize, stackOrigin, shift)
        }
    }
}

@Composable
private fun MenuStack(
    state: MessageMenuState,
    anchor: MenuAnchor,
    quickReactions: List<String>,
    actionGroups: List<List<MenuAction>>,
    iconSize: Dp = MenuIconSize,
    origin: Offset,
    shift: MutableFloatState,
) {
    val outgoing = anchor.msg.outgoing
    val pivot = TransformOrigin(if (outgoing) 1f else 0f, 0.1f)
    // Set by the layout below: a card placed above the bubble grows from its bottom corner.
    val flipped = remember { mutableStateOf(false) }
    // Pop is read only inside graphicsLayer, so frames never recompose the stack.
    val entrance = Modifier.graphicsLayer {
        val p = state.pop.value
        alpha = p.coerceIn(0f, 1f)
        scaleX = 0.75f + 0.25f * p
        scaleY = 0.75f + 0.25f * p
        transformOrigin = pivot
    }
    val cardEntrance = Modifier.graphicsLayer {
        val p = state.pop.value
        alpha = p.coerceIn(0f, 1f)
        scaleX = 0.75f + 0.25f * p
        scaleY = 0.75f + 0.25f * p
        transformOrigin = TransformOrigin(pivot.pivotFractionX, if (flipped.value) 0.9f else 0.1f)
    }

    Layout(
        content = {
            ReactionStrip(state, anchor.msg, quickReactions, entrance)
            MenuCard(
                groups = actionGroups,
                hovered = state.hovered - quickReactions.size,
                modifier = cardEntrance,
                itemHeight = 42.dp,
                iconSize = iconSize,
                onRowPositioned = { i, coords -> state.targets[quickReactions.size + i] = coords },
                onPick = { it.onClick() },
            )
        },
        modifier = Modifier.fillMaxSize(),
    ) { measurables, constraints ->
        val loose = constraints.copy(minWidth = 0, minHeight = 0)
        val strip = measurables[0].measure(loose)
        val card = measurables[1].measure(loose)

        layout(constraints.maxWidth, constraints.maxHeight) {
            val margin = 12.dp.roundToPx()
            val gap = 6.dp.roundToPx()
            val top = (anchor.bounds.top - origin.y).roundToInt()
            val bottom = (anchor.bounds.bottom - origin.y).roundToInt()
            fun xFor(w: Int) = if (outgoing) constraints.maxWidth - margin - w else margin

            // A low bubble keeps its place with the card above and the strip below. Only a
            // bubble that fits neither way shifts into [shift], and then the card wins.
            val maxBottom = constraints.maxHeight - margin - gap - card.height
            val flip = bottom > maxBottom
            flipped.value = flip
            val minTop = margin + gap + if (flip) card.height else strip.height
            var dy = 0
            if (top < minTop) dy = minTop - top
            if (flip) {
                val stripMax = constraints.maxHeight - margin - gap - strip.height
                if (bottom + dy > stripMax) dy = stripMax - bottom
            } else if (bottom + dy > maxBottom) dy = maxBottom - bottom
            shift.floatValue = dy.toFloat()

            if (flip) {
                card.place(xFor(card.width), (top + dy - gap - card.height).coerceAtLeast(margin))
                strip.place(xFor(strip.width), bottom + dy + gap)
            } else {
                strip.place(xFor(strip.width), (top + dy - gap - strip.height).coerceAtLeast(margin))
                card.place(xFor(card.width), bottom + dy + gap)
            }
        }
    }
}

@Composable
private fun ReactionStrip(
    state: MessageMenuState,
    msg: UiMessage,
    emojis: List<String>,
    modifier: Modifier,
) {
    val colors = MaterialTheme.colorScheme
    Row(
        modifier
            .clip(RoundedCornerShape(24.dp))
            .background(colors.surfaceContainerHigh)
            .padding(horizontal = 5.dp, vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        emojis.forEachIndexed { i, emoji ->
            val chipPop = remember { Animatable(0f) }
            LaunchedEffect(Unit) {
                delay((40L + 30L * i).milliseconds)
                chipPop.animateTo(1f, tween(220, easing = Overshoot))
            }
            val mine = msg.reactions.any { it.emoji == emoji && it.mine }
            val hoveredHere = state.hovered == i
            val hover by animateFloatAsState(if (hoveredHere) 1.25f else 1f, spring())
            Box(
                Modifier
                    .onGloballyPositioned { state.targets[i] = it }
                    .graphicsLayer {
                        val s = chipPop.value * hover
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
                    .clickable { state.react(emoji) }
                    .padding(horizontal = 6.dp, vertical = 4.dp),
            ) {
                EmojiText(emoji, fontSize = 19.sp)
            }
        }
    }
}
