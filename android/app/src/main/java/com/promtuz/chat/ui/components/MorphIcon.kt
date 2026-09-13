package com.promtuz.chat.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.material3.LocalContentColor
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.rotate
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.stage.ChatMotion
import kotlin.math.hypot

/** Shared motion silhouettes on a 24-unit canvas. Unused strokes collapse to a point. */
enum class MorphGlyph(internal val strokes: List<Float>, internal val angle: Float = 0f) {
    Back(listOf(5f,12f,12f,5f, 5f,12f,12f,19f, 5f,12f,19f,12f)),
    ChevronLeft(listOf(8f,12f,15f,5f, 8f,12f,15f,19f, 12f,12f,12f,12f)),
    ChevronRight(ChevronLeft.strokes, 180f),
    ChevronUp(ChevronLeft.strokes, 90f),
    ChevronDown(ChevronLeft.strokes, -90f),
    Close(listOf(6f,6f,18f,18f, 6f,18f,18f,6f, 12f,12f,12f,12f)),
    Plus(Close.strokes, 45f),
    Play(listOf(7f,5f,7f,19f, 7f,5f,19f,12f, 7f,19f,19f,12f)),
    Pause(listOf(8f,5f,8f,19f, 16f,5f,16f,19f, 12f,12f,12f,12f)),
}

/** Hoist this above conditional layouts when the control moves between modes. */
class MorphIconState internal constructor(
    internal val coordinates: List<State<Float>>,
    internal val angle: State<Float>,
)

@Composable
fun rememberMorphIconState(glyph: MorphGlyph): MorphIconState {
    // State reads stay in Canvas: the surrounding toolbar does not recompose per frame.
    // animateFloatAsState starts a retargeted motion at its current value, so rapid
    // toggle/back/toggle sequences never reset the icon to an endpoint.
    val coordinates = glyph.strokes.mapIndexed { index, value ->
        animateFloatAsState(value, ChatMotion.spec(), label = "icon coordinate $index")
    }
    var previous by remember { mutableFloatStateOf(glyph.angle) }
    val rotation = remember(glyph) {
        previous + ((glyph.angle - previous + 540f) % 360f - 180f)
    }
    SideEffect { previous = rotation }
    val angle = animateFloatAsState(rotation, ChatMotion.spec(), label = "icon rotation")
    return remember { MorphIconState(coordinates, angle) }
}

@Composable
fun MorphIcon(glyph: MorphGlyph, description: String?, modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current) =
    MorphIcon(rememberMorphIconState(glyph), description, modifier, tint)

@Composable
fun MorphIcon(state: MorphIconState, description: String?, modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current) {
    Canvas(modifier.size(24.dp).semantics { if (description != null) contentDescription = description }) {
        val unit = minOf(size.width, size.height) / 24f
        val origin = Offset((size.width - 24f * unit) / 2, (size.height - 24f * unit) / 2)
        rotate(state.angle.value) {
            repeat(3) { i ->
                val c = state.coordinates
                val x1 = c[i * 4].value; val y1 = c[i * 4 + 1].value
                val x2 = c[i * 4 + 2].value; val y2 = c[i * 4 + 3].value
                val alpha = (hypot(x2 - x1, y2 - y1) / 2f).coerceIn(0f, 1f)
                if (alpha > 0f) drawLine(tint.copy(alpha = tint.alpha * alpha),
                    origin + Offset(x1 * unit, y1 * unit), origin + Offset(x2 * unit, y2 * unit),
                    strokeWidth = 2f * unit, cap = StrokeCap.Round)
            }
        }
    }
}
