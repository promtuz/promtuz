package com.promtuz.chat.ui.animation

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.EaseInOutCirc
import androidx.compose.animation.core.EaseOutExpo
import androidx.compose.animation.core.VectorConverter
import androidx.compose.animation.core.tween
import androidx.compose.foundation.layout.Box
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.Text
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.MotionDurationScale
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.text
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.constrainHeight
import androidx.compose.ui.unit.constrainWidth
import androidx.compose.ui.util.lerp
import com.promtuz.chat.ui.constants.Tweens
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.flow.collectLatest
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.roundToInt

/**
 * A clipped strip of stacked values, eased as one sweep rather than easing each item separately.
 * Retargeting preserves the visible pair and their fractional offset. The initial sweep deadline
 * is retained throughout a burst, so new arrivals speed it up instead of extending the queue.
 * [minDurationMillis] controls sampling density, not a per-item delay; large jumps skip values.
 * The time budget respects Android's animation-duration scale.
 */
@Composable
fun <T> QueuedAnimatedContent(
    targetState: T,
    modifier: Modifier = Modifier,
    durationMillis: Int = 300,
    minDurationMillis: Int = minOf(16, durationMillis),
    progression: ContentProgression<T>? = null,
    contentAlignment: Alignment = Alignment.CenterStart,
    label: String = "queued content",
    maxDurationMillis: Int = 300,
    content: @Composable (T) -> Unit,
) {
    QueuedContentStrip(targetState, modifier, durationMillis, minDurationMillis, progression,
        contentAlignment, label, maxDurationMillis, fixedContent = {}) { value, _ -> content(value) }
}

/** Keeps the common leading digits still; the suffix moves together on the shared strip. */
@Composable
fun QueuedAnimatedNumber(
    number: Int,
    modifier: Modifier = Modifier,
    durationMillis: Int = 300,
    maxDurationMillis: Int = 300,
    contentAlignment: Alignment = Alignment.CenterStart,
    label: String = "queued number",
    style: TextStyle = LocalTextStyle.current,
    color: Color = Color.Unspecified,
) {
    val numberStyle = style.copy(fontFeatureSettings = listOfNotNull(style.fontFeatureSettings, "tnum").joinToString(", "))
    QueuedContentStrip(number, modifier.clearAndSetSemantics { text = AnnotatedString(number.toString()) },
        durationMillis, minOf(16, durationMillis),
        ContentProgression.Integers, contentAlignment, label, maxDurationMillis,
        fixedContent = { values ->
            Text(numberPrefix(values), style = numberStyle, color = color, maxLines = 1,
                overflow = TextOverflow.Clip)
        }) { value, values ->
        Text(value.toString().drop(numberPrefix(values).length), style = numberStyle,
            color = color, maxLines = 1, overflow = TextOverflow.Clip)
    }
}

private fun numberPrefix(values: List<Int>): String {
    val strings = values.map(Int::toString)
    // Carries that change the number of places move the whole number.
    if (strings.any { it.length != strings.first().length }) return ""
    return strings.drop(1).fold(strings.first()) { prefix, value -> prefix.commonPrefixWith(value) }
}

@Composable
private fun <T> QueuedContentStrip(
    targetState: T,
    modifier: Modifier,
    durationMillis: Int,
    minDurationMillis: Int,
    progression: ContentProgression<T>?,
    contentAlignment: Alignment,
    label: String,
    maxDurationMillis: Int,
    fixedContent: @Composable (List<T>) -> Unit,
    content: @Composable (T, List<T>) -> Unit,
) {
    require(durationMillis > 0 && minDurationMillis in 1..durationMillis && maxDurationMillis > 0)
    val queue = remember { ContentQueue(targetState) }
    val position = remember { Animatable(0f, Float.VectorConverter, label = label) }
    var entries by remember { mutableStateOf(queue.entries) }
    val requested by rememberUpdatedState(targetState)
    val baseDuration by rememberUpdatedState(durationMillis)
    val minimumDuration by rememberUpdatedState(minDurationMillis)
    val maximumDuration by rememberUpdatedState(maxDurationMillis)
    val currentProgression by rememberUpdatedState(progression)

    LaunchedEffect(queue) {
        var deadline: Long? = null
        snapshotFlow { requested }.collectLatest { value ->
            if (entries.size == 1 && entries.single().value == value) return@collectLatest
            val now = System.nanoTime()
            val scale = currentCoroutineContext()[MotionDurationScale]?.scaleFactor ?: 1f
            val budget = minOf(baseDuration, maximumDuration)
            if (deadline == null) deadline = now + (budget * scale.toDouble() * 1_000_000).toLong()
            val remaining = if (scale > 0f) {
                ceil((deadline!! - now).coerceAtLeast(0L) / (1_000_000.0 * scale))
                    .toInt().coerceIn(0, budget)
            } else 0
            val samples = (remaining / minimumDuration).coerceIn(1, 16)
            val sweep = queue.retarget(position.value, value, currentProgression, samples)
            // collectLatest has stopped the old animation. Rebasing keeps the same visible
            // entries at the same offsets; their stable IDs also preserve their composition.
            position.snapTo(sweep.start)
            entries = sweep.entries
            if (remaining > 0 && sweep.start != sweep.target) {
                position.animateTo(sweep.target, tween(remaining, easing = EaseOutExpo))
            }
            // No finally block: cancellation must retain the in-flight position for retargeting.
            position.snapTo(0f)
            entries = queue.settle(value)
            deadline = null
        }
    }

    Layout(
        modifier = modifier.clipToBounds(),
        content = {
            val values = entries.map { it.value }
            Box { fixedContent(values) }
            entries.forEach { entry ->
                key(entry.id) {
                    Box(if (entry.value == targetState) Modifier else Modifier.clearAndSetSemantics {}) {
                        content(entry.value, values)
                    }
                }
            }
        },
    ) { measurables, constraints ->
        val loose = constraints.copy(minWidth = 0, minHeight = 0)
        val fixed = measurables.first().measure(loose)
        val movingConstraints = loose.copy(maxWidth = (loose.maxWidth - fixed.width).coerceAtLeast(0))
        val children = measurables.drop(1).map { it.measure(movingConstraints) }
        val offset = position.value.coerceIn(0f, children.lastIndex.toFloat())
        val first = floor(offset).toInt()
        val next = minOf(first + 1, children.lastIndex)
        val fraction = offset - first
        val movingWidth = lerp(children[first].width, children[next].width, fraction)
        val width = fixed.width + movingWidth
        val height = maxOf(fixed.height, lerp(children[first].height, children[next].height, fraction))
        val viewport = IntSize(constraints.constrainWidth(width), constraints.constrainHeight(height))
        val tops = IntArray(children.size)
        for (index in 1 until children.size) tops[index] = tops[index - 1] + children[index - 1].height
        val scrollY = tops[first] + children[first].height * fraction
        val origin = contentAlignment.align(IntSize(width, height), viewport, layoutDirection)
        val originY = origin.y
        layout(viewport.width, viewport.height) {
            fixed.place(origin.x, originY)
            children.forEachIndexed { index, child ->
                val y = originY + (tops[index] - scrollY).roundToInt()
                // Offscreen entries remain in the strip, without painting or exposing semantics.
                if (y < viewport.height && y + child.height > 0) {
                    val suffixSpace = IntSize(movingWidth, height)
                    val x = origin.x + fixed.width + contentAlignment.align(
                        IntSize(child.width, child.height), suffixSpace, layoutDirection).x
                    child.place(x, y)
                }
            }
        }
    }
}
