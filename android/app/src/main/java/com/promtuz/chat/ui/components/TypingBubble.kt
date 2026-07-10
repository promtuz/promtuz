package com.promtuz.chat.ui.components

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import kotlin.math.PI
import kotlin.math.sin

/**
 * The peer-is-typing row: an incoming-shaped bubble with three dots pulsing on a
 * staggered sine. Lives at the bottom of the list while the signal is live; its
 * exit as the real message enters reads as the typing→message hand-off.
 */
@Composable
fun TypingBubble(modifier: Modifier = Modifier) {
    val appearance = LocalChatAppearance.current
    val chat = LocalChatColors.current
    val shape = rememberBubbleShape(
        outgoing = false,
        mergedTop = false,
        mergedBottom = false,
        style = appearance.bubble,
    )

    val phase by rememberInfiniteTransition(label = "typing")
        .animateFloat(0f, 1f, infiniteRepeatable(tween(900, easing = LinearEasing)), label = "phase")

    Box(modifier.fillMaxWidth().padding(horizontal = 12.dp)) {
        Row(
            Modifier
                .align(Alignment.CenterStart)
                .clip(shape)
                .background(chat.incomingBubble)
                .padding(horizontal = 13.dp, vertical = 11.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            repeat(3) { i ->
                Box(
                    Modifier
                        .size(7.dp)
                        .graphicsLayer {
                            val t = sin(2f * PI.toFloat() * (phase - i * 0.15f))
                            alpha = 0.35f + 0.4f * (t * 0.5f + 0.5f)
                            val s = 0.8f + 0.2f * (t * 0.5f + 0.5f)
                            scaleX = s
                            scaleY = s
                        }
                        .clip(CircleShape)
                        .background(chat.onIncomingBubble.copy(alpha = 0.65f)),
                )
            }
        }
    }
}
