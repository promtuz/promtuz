package com.promtuz.chat.ui.components

import androidx.compose.runtime.Composable
import androidx.compose.foundation.background
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.addOutline
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Paint
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.drawOutline
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.drawscope.scale
import com.promtuz.chat.ui.stage.LocalStageBubbleMotion
import kotlin.math.PI
import kotlin.math.sin
import androidx.compose.ui.unit.dp

/** Keep a single surface at the tail while dots give way to the complete message. */
@Composable
internal fun Modifier.typingMorphSurface(shape: Shape, surface: Color, foreground: Color, enabled: Boolean): Modifier {
    val motion = LocalStageBubbleMotion.current
    val source = motion?.source
    if (!enabled) return this
    if (source == null) return background(surface, shape)
    motion.drawsMorph = true
    return drawWithContent {
        val progress = motion.progress()
        if (progress >= 1f) {
            drawOutline(shape.createOutline(size, layoutDirection, this), surface)
            drawContent()
        } else {
            val width = source.size.width + (size.width - source.size.width) * progress
            val height = source.size.height + (size.height - source.size.height) * progress
            val top = size.height - height
            translate(top = top) {
                drawOutline(shape.createOutline(Size(width, height), layoutDirection, this),
                    surface.copy(alpha = surface.alpha * (source.opacity + (1f - source.opacity) * progress)))
                // Dots keep their size and position, rather than stretching into letters.
                val dotsAlpha = (1f - progress * 3f).coerceIn(0f, 1f)
                val dotScale = (source.size.height / 29.dp.toPx()).coerceAtMost(1f)
                repeat(3) { i ->
                    val pulse = sin(2f * PI.toFloat() * (source.dotsPhase - i * 0.15f)) * 0.5f + 0.5f
                    drawCircle(foreground.copy(alpha = 0.65f * (0.35f + 0.4f * pulse) * dotsAlpha * source.opacity),
                        radius = 3.5.dp.toPx() * (0.8f + 0.2f * pulse) * dotScale,
                        center = Offset((16.5.dp.toPx() + i * 11.dp.toPx()) * dotScale, 14.5.dp.toPx() * dotScale))
                }
            }
            // Text, media and timestamp scale together without rewrapping or distortion.
            val contentAlpha = ((progress - 0.15f) / 0.85f).coerceIn(0f, 1f)
            val clip = Path().apply {
                addOutline(shape.createOutline(Size(width, height), layoutDirection, this@drawWithContent))
                translate(Offset(0f, top))
            }
            clipPath(clip) {
                val canvas = drawContext.canvas
                canvas.saveLayer(Rect(Offset.Zero, size), Paint().apply { alpha = contentAlpha })
                scale(minOf(width / size.width, height / size.height), pivot = Offset(0f, size.height)) {
                    this@drawWithContent.drawContent()
                }
                canvas.restore()
            }
        }
    }
}
