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
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
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
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.domain.model.UiMessage
import kotlin.math.roundToInt
import kotlinx.coroutines.delay

/** What was long-pressed: the message, its row bounds in root space, its merge shape. */
data class MenuAnchor(
    val msg: UiMessage,
    val bounds: Rect,
    val mergedTop: Boolean,
    val mergedBottom: Boolean,
)

private val Overshoot = Easing { OvershootInterpolator(1.1f).getInterpolation(it) }

/**
 * The long-press overlay, Telegram-mechanics: the list hides the pressed row and
 * this re-composes the same bubble at its captured bounds over a 20% scrim (320ms
 * EaseOutQuint) — a contrast "lift" with zero reparenting. Reaction strip + action
 * card pop in (250ms overshoot, staggered chips) anchored to the bubble's side,
 * flipping above when there's no room below.
 */
@Composable
fun MessageContextMenu(
    anchor: MenuAnchor,
    quickReactions: List<String>,
    actions: List<MenuAction>,
    onReact: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    val scrim = remember { Animatable(0f) }
    val enter = remember { Animatable(0f) }
    LaunchedEffect(Unit) { scrim.animateTo(0.2f, tween(320, easing = EaseOutQuint)) }
    LaunchedEffect(Unit) { enter.animateTo(1f, tween(250, easing = Overshoot)) }

    // Root offset makes bounds (captured in window-root space) local to this overlay.
    var origin by remember { mutableStateOf(Offset.Zero) }

    Box(Modifier.fillMaxSize().onGloballyPositioned { origin = it.positionInRoot() }) {
        Box(
            Modifier
                .fillMaxSize()
                .graphicsLayer { alpha = scrim.value }
                .background(Color.Black)
                .pointerInput(Unit) { detectTapGestures { onDismiss() } },
        )

        // The lifted bubble, pixel-identical at its on-screen position.
        Box(
            Modifier.offset {
                IntOffset(
                    (anchor.bounds.left - origin.x).roundToInt(),
                    (anchor.bounds.top - origin.y).roundToInt(),
                )
            },
        ) {
            MessageBubble(msg = anchor.msg, mergedTop = anchor.mergedTop, mergedBottom = anchor.mergedBottom)
        }

        MenuStack(anchor, quickReactions, actions, enter.value, origin, onReact)
    }
}

/** Measures strip + card, places them around the bubble (below; flips above if cramped). */
@Composable
private fun MenuStack(
    anchor: MenuAnchor,
    quickReactions: List<String>,
    actions: List<MenuAction>,
    enter: Float,
    origin: Offset,
    onReact: (String) -> Unit,
) {
    val outgoing = anchor.msg.outgoing
    val pivot = TransformOrigin(if (outgoing) 1f else 0f, 0.1f)
    val entrance = Modifier.graphicsLayer {
        alpha = enter.coerceIn(0f, 1f)
        scaleX = 0.75f + 0.25f * enter
        scaleY = 0.75f + 0.25f * enter
        transformOrigin = pivot
    }

    Layout(
        content = {
            ReactionStrip(anchor.msg, quickReactions, entrance, onReact)
            MenuCard(actions, entrance)
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
            val pop = remember { Animatable(0f) }
            LaunchedEffect(Unit) {
                delay(40L + 30L * i)
                pop.animateTo(1f, tween(220, easing = Overshoot))
            }
            val mine = msg.reactions.any { it.emoji == emoji && it.mine }
            Box(
                Modifier
                    .graphicsLayer {
                        alpha = pop.value.coerceIn(0f, 1f)
                        scaleX = pop.value
                        scaleY = pop.value
                    }
                    .clip(CircleShape)
                    .background(if (mine) colors.primary.copy(alpha = 0.22f) else Color.Transparent)
                    .clickable { onReact(emoji) }
                    .padding(horizontal = 7.dp, vertical = 5.dp),
            ) {
                Text(emoji, fontSize = 21.sp)
            }
        }
    }
}

@Composable
private fun MenuCard(actions: List<MenuAction>, entrance: Modifier) {
    val colors = MaterialTheme.colorScheme
    Column(
        entrance
            .width(IntrinsicSize.Max)
            .widthIn(min = 190.dp)
            .clip(RoundedCornerShape(16.dp))
            .background(colors.surfaceContainerHigh),
    ) {
        actions.forEach { action ->
            val tint = if (action.destructive) colors.error else colors.onSurface
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable(onClick = action.onClick)
                    .padding(horizontal = 16.dp, vertical = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                action.icon?.let { DrawableIcon(it, Modifier.size(19.dp), tint = tint) }
                Text(
                    action.label,
                    Modifier.padding(start = 14.dp),
                    color = tint,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
        }
    }
}
