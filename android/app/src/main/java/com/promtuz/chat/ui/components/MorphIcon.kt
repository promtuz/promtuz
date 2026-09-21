package com.promtuz.chat.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.material3.LocalContentColor
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.rotate
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.stage.ChatMotion
import kotlin.math.abs
import kotlin.math.cos
import kotlin.math.sin

// Each contour has five vertices followed by the three interior corner setbacks.
// Duplicate end vertices let lines and bends share the rounded triangle's topology.
private fun bend(x1: Float, y1: Float, x2: Float, y2: Float, x3: Float, y3: Float,
    setback: Float = 1.2f) = listOf(x1, y1, x1, y1, x2, y2, x3, y3, x3, y3, 0f, setback, 0f)
private fun line(x1: Float, y1: Float, x2: Float, y2: Float) =
    bend(x1, y1, (x1 + x2) / 2, (y1 + y2) / 2, x2, y2, 0f)
private val hiddenStroke = line(12f, 12f, 12f, 12f)

/**
 * Rounded centerlines on a 24-unit canvas; SVG sources: design/icons/outlined/morph.
 * Two compatible contours retain their identities during interrupted and reversed motion.
 * Directional bends use a smaller setback to preserve their tips at small sizes.
 */
enum class MorphGlyph(internal val strokes: List<Float>, internal val angle: Float = 0f) {
    Back(bend(12f, 2.375f, 2.375f, 12f, 12f, 21.625f) + line(2.8f, 12f, 21.625f, 12f)),
    ChevronLeft(bend(16.8125f, 2.375f, 7.1875f, 12f, 16.8125f, 21.625f) + hiddenStroke),
    ChevronRight(ChevronLeft.strokes, 180f),
    ChevronUp(ChevronLeft.strokes, 90f),
    ChevronDown(ChevronLeft.strokes, -90f),
    // Match the plus's stroke length, rather than filling the square along both diagonals.
    Close(line(5.1941f, 5.1941f, 18.8059f, 18.8059f) + line(5.1941f, 18.8059f, 18.8059f, 5.1941f)),
    Plus(Close.strokes, 45f),
    Check(bend(2.375f, 12f, 8.79167f, 18.41667f, 21.625f, 5.58333f) + hiddenStroke),
    Code(bend(8.875f, 5.5f, 2.375f, 12f, 8.875f, 18.5f) + bend(15.125f, 5.5f, 21.625f, 12f, 15.125f, 18.5f)),
    Play(listOf(4.5f, 12f, 4.5f, 2.375f, 21.625f, 12f, 4.5f, 21.625f, 4.5f, 12f, 2f, 1.2f, 2f) + hiddenStroke),
    Pause(line(6.5f, 2.375f, 6.5f, 21.625f) + line(17.5f, 2.375f, 17.5f, 21.625f)),
}

/** Hoist this above conditional layouts when the control moves between modes. */
class MorphIconState internal constructor(
    internal val coordinates: List<State<Float>>,
    internal val angle: State<Float>,
)

@Composable
fun rememberMorphIconState(glyph: MorphGlyph): MorphIconState {
    // Read animation state only while drawing. Retargeting starts from the current
    // values, including the corner setbacks, so interrupted motions do not reset.
    val coordinates = glyph.strokes.mapIndexed { index, value ->
        animateFloatAsState(value, ChatMotion.spec(), label = "icon coordinate $index")
    }
    var previous by remember { mutableFloatStateOf(glyph.angle) }
    val rotation = remember(glyph) {
        previous + (((glyph.angle - previous + 180f) % 360f + 360f) % 360f - 180f)
    }
    SideEffect { previous = rotation }
    val angle = animateFloatAsState(rotation, ChatMotion.spec(), label = "icon rotation")
    return remember { MorphIconState(coordinates, angle) }
}

/** [strokeWidth] is a fixed dp thickness, independent of the icon's layout size. */
@Composable
fun MorphIcon(glyph: MorphGlyph, description: String?, modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current, strokeWidth: Dp = 1.75.dp) =
    MorphIcon(rememberMorphIconState(glyph), description, modifier, tint, strokeWidth)

/**
 * [strokeWidth] stays fixed as the layout size changes. Zero draws nothing.
 * The centerlines fit inside the remaining space after the stroke and proportional
 * outer padding; a canvas too small for that thickness draws nothing.
 */
@Composable
fun MorphIcon(state: MorphIconState, description: String?, modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current, strokeWidth: Dp = 1.75.dp) {
    require(strokeWidth.value.isFinite() && strokeWidth >= 0.dp) {
        "strokeWidth must be finite and non-negative"
    }
    val path = remember { Path() }
    Canvas(modifier.size(24.dp).semantics { if (description != null) contentDescription = description }) {
        val unit = minOf(size.width, size.height) / 24f
        val strokePx = strokeWidth.toPx()
        if (unit <= 0f || strokePx == 0f) return@Canvas
        val availableExtent = 10.5f * unit - strokePx / 2f
        if (availableExtent <= 0f) return@Canvas
        val origin = Offset((size.width - 24f * unit) / 2, (size.height - 24f * unit) / 2)
        val center = Offset(12f, 12f)
        // A turning, partially morphed contour can be wider than either endpoint.
        // Fit its control hull without changing stroke weight or clipping the turn.
        val radians = Math.toRadians(state.angle.value.toDouble())
        val cosine = cos(radians).toFloat()
        val sine = sin(radians).toFloat()
        var extent = 9.625f
        repeat(2) { contour ->
            repeat(5) { vertex ->
                val index = contour * 13 + vertex * 2
                val x = state.coordinates[index].value - 12f
                val y = state.coordinates[index + 1].value - 12f
                extent = maxOf(extent, abs(x * cosine - y * sine), abs(x * sine + y * cosine))
            }
        }
        val fit = minOf(9.625f, availableExtent / unit) / extent
        fun pixel(point: Offset) = origin + (center + (point - center) * fit) * unit
        rotate(state.angle.value) {
            repeat(2) { contour ->
                val base = contour * 13
                val points = List(5) { i ->
                    Offset(state.coordinates[base + i * 2].value, state.coordinates[base + i * 2 + 1].value)
                }
                val length = (1..4).sumOf { (points[it] - points[it - 1]).getDistance().toDouble() }.toFloat()
                val alpha = (length / 2f).coerceIn(0f, 1f)
                if (alpha == 0f) return@repeat
                path.reset()
                val start = pixel(points.first())
                path.moveTo(start.x, start.y)
                for (i in 1..3) {
                    val corner = points[i]
                    val incoming = points[i - 1] - corner
                    val outgoing = points[i + 1] - corner
                    val before = incoming.getDistance()
                    val after = outgoing.getDistance()
                    val setback = minOf(state.coordinates[base + 9 + i].value, before / 2f, after / 2f)
                        .coerceAtLeast(0f)
                    val control = pixel(corner)
                    if (setback == 0f) {
                        path.lineTo(control.x, control.y)
                    } else {
                        val entry = pixel(corner + incoming * (setback / before))
                        val exit = pixel(corner + outgoing * (setback / after))
                        path.lineTo(entry.x, entry.y)
                        path.quadraticTo(control.x, control.y, exit.x, exit.y)
                    }
                }
                val end = pixel(points.last())
                path.lineTo(end.x, end.y)
                drawPath(path, tint.copy(alpha = tint.alpha * alpha),
                    style = Stroke(strokePx, cap = StrokeCap.Round, join = StrokeJoin.Round))
            }
        }
    }
}
