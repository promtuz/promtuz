package com.promtuz.chat.ui.components

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animate
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Modifier
import androidx.compose.ui.MotionDurationScale
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.lerp
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.CompositingStrategy
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.scale
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.ui.stage.ChatMotion
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.sin
import kotlin.coroutines.coroutineContext
import kotlinx.coroutines.flow.first

/** Status silhouettes share two hands/strokes, a ring, and a separating copy of the tick. */
enum class MessageStatusGlyph(val description: String) {
    Sending("Sending"), Sent("Sent"), Delivered("Delivered"), Seen("Seen"), Failed("Failed to send");

    companion object {
        fun from(status: SendStatus): MessageStatusGlyph = when (status) {
            SendStatus.Pending -> Sending
            SendStatus.Sent -> Sent
            SendStatus.Delivered -> Delivered
            SendStatus.Read -> Seen
            SendStatus.Failed -> Failed
        }
    }
}

/**
 * Keep this composition keyed to the message, not its status. Existing messages start at their
 * current silhouette; changes morph from the last drawn pose, including interrupted clock hands.
 */
@Composable
fun MessageStatusIcon(
    status: SendStatus,
    modifier: Modifier = Modifier,
    tint: Color,
    seenTint: Color,
    errorTint: Color = MaterialTheme.colorScheme.error,
) = MessageStatusIcon(MessageStatusGlyph.from(status), modifier, tint, seenTint, errorTint)

@Composable
fun MessageStatusIcon(
    glyph: MessageStatusGlyph,
    modifier: Modifier = Modifier,
    tint: Color,
    seenTint: Color,
    errorTint: Color = MaterialTheme.colorScheme.error,
) {
    // Seen changes only the ink. A receipt arriving mid-split must not restart the geometry.
    val shape = if (glyph == MessageStatusGlyph.Seen) MessageStatusGlyph.Delivered else glyph
    val motion = remember { StatusIconMotion(shape) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(shape, lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) { motion.moveTo(shape) }
    }
    val color = animateColorAsState(
        when (glyph) {
            MessageStatusGlyph.Seen -> seenTint
            MessageStatusGlyph.Failed -> errorTint
            else -> tint
        },
        ChatMotion.spec(), label = "message status ink",
    )
    val front = remember { Path() }
    val back = remember { Path() }
    Canvas(modifier.size(16.dp).semantics { contentDescription = glyph.description }
        .graphicsLayer {
            // Clear cuts only this icon's backing layer, revealing even photographic backgrounds.
            // Apply opacity once so coincident strokes never become darker during division.
            compositingStrategy = CompositingStrategy.Offscreen
            alpha = color.value.alpha
        }) {
        val pose = motion.pose
        val ink = color.value.copy(alpha = 1f)
        val unit = minOf(size.width, size.height) / 24f
        translate((size.width - 24f * unit) / 2, (size.height - 24f * unit) / 2) {
            scale(unit, unit, pivot = Offset.Zero) {
                val stroke = Stroke(width = 2.25f, cap = StrokeCap.Round, join = StrokeJoin.Round)
                val separation = 2.5f * pose.split
                front.setHands(pose, -separation)
                if (pose.split > 0f) {
                    back.setHands(pose, separation, rear = true)
                    drawPath(back, ink, style = stroke)
                    drawPath(front, Color.Black,
                        style = Stroke(2f + 2.5f * pose.split, cap = StrokeCap.Round, join = StrokeJoin.Round),
                        blendMode = BlendMode.Clear)
                }
                if (pose.ring > 0f) {
                    drawCircle(ink, radius = 8.75f, center = Center, alpha = pose.ring, style = stroke)
                }
                drawPath(front, ink, style = stroke)
            }
        }
    }
}

private val Center = Offset(12f, 12f)

private data class StatusPose(
    val shortStart: Offset,
    val shortEnd: Offset,
    val longStart: Offset,
    val longEnd: Offset,
    val ring: Float = 0f,
    val split: Float = 0f,
)

private fun Path.setHands(pose: StatusPose, dx: Float, rear: Boolean = false) {
    reset()
    // Grow the rear short arm only as far as its crossing with the foreground tick.
    // A full duplicated arm would leave a detached round cap beyond the clearance mask.
    val tip = if (rear) lerp(pose.shortStart, pose.shortEnd, 0.625f * pose.split) else pose.shortEnd
    moveTo(tip.x + dx, tip.y)
    lineTo(pose.shortStart.x + dx, pose.shortStart.y)
    if (pose.shortStart != pose.longStart) moveTo(pose.longStart.x + dx, pose.longStart.y)
    lineTo(pose.longEnd.x + dx, pose.longEnd.y)
}

private fun clockPose(turn: Float): StatusPose {
    fun hand(angle: Float, length: Float): Offset {
        val radians = angle * PI / 180.0
        return Center + Offset(cos(radians).toFloat(), sin(radians).toFloat()) * length
    }
    // Start at 10:10. The minute hand laps every two seconds, the hour hand every 24.
    return StatusPose(Center, hand(-145f + turn * 360f, 4.2f),
        Center, hand(-30f + turn * 4320f, 6.2f), ring = 1f)
}

private fun restingPose(glyph: MessageStatusGlyph): StatusPose = when (glyph) {
    MessageStatusGlyph.Sending -> clockPose(0f)
    MessageStatusGlyph.Failed -> StatusPose(
        Offset(12f, 6.5f), Offset(12f, 12.5f),
        // A tiny round-capped segment remains a dot on every Canvas backend.
        Offset(12f, 17f), Offset(12f, 17.001f), ring = 1f,
    )
    else -> StatusPose(Offset(9f, 16f), Offset(5f, 12f), Offset(9f, 16f), Offset(19f, 6f),
        split = if (glyph == MessageStatusGlyph.Sent) 0f else 1f)
}

private class StatusIconMotion(initial: MessageStatusGlyph) {
    var pose by mutableStateOf(restingPose(initial))
        private set
    private var clockTurn = 0f

    suspend fun moveTo(glyph: MessageStatusGlyph) {
        val source = pose
        val target = if (glyph == MessageStatusGlyph.Sending) clockPose(clockTurn) else restingPose(glyph)
        if (source != target) {
            animate(0f, 1f, animationSpec = ChatMotion.spec()) { progress, _ ->
                // A skipped Sent stage still reads as hands becoming a tick, then dividing,
                // with overlapping motion and no artificial pause at the intermediate state.
                val division = if (source.ring > 0f && target.split > source.split)
                    ((progress - 0.25f) / 0.75f).coerceIn(0f, 1f) else progress
                pose = StatusPose(
                    lerp(source.shortStart, target.shortStart, progress),
                    lerp(source.shortEnd, target.shortEnd, progress),
                    lerp(source.longStart, target.longStart, progress),
                    lerp(source.longEnd, target.longEnd, progress),
                    source.ring + (target.ring - source.ring) * progress,
                    source.split + (target.split - source.split) * division,
                )
            }
        }
        if (glyph == MessageStatusGlyph.Sending) {
            // Compose's animation clock observes system animation scaling; no custom busy loop.
            // Lifecycle cancellation preserves both the current geometry and the hand phase.
            val durationScale = coroutineContext[MotionDurationScale]
            while (true) {
                // Infinite animate finishes when animations are disabled. Wait without drawing,
                // then resume the same clock if the setting changes while this row stays visible.
                snapshotFlow { durationScale?.scaleFactor ?: 1f }.first { it > 0f }
                animate(clockTurn, clockTurn + 1f,
                    animationSpec = infiniteRepeatable(tween(24_000, easing = LinearEasing))) { turn, _ ->
                    if (durationScale?.scaleFactor != 0f) {
                        clockTurn = turn % 1f
                        pose = clockPose(clockTurn)
                    }
                }
            }
        }
    }
}
