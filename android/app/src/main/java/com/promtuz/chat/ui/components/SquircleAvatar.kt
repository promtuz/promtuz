package com.promtuz.chat.ui.components

import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Shader
import android.graphics.Typeface
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.GenericShape
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Matrix
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathOperation
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.asAndroidPath
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.theme.DepthPalette

/** Which lighting layers to draw. Every layer assumes light from the top left. */
data class AvatarLayers(
    val base: Boolean = true,
    val rim: Boolean = true,
    val innerShadow: Boolean = true,
    val specular: Boolean = true,
    val dropShadow: Boolean = true,
    val glyphLighting: Boolean = true,
) {
    companion object {
        val All = AvatarLayers()
        val Flat = AvatarLayers(
            base = false,
            rim = false,
            innerShadow = false,
            specular = false,
            dropShadow = false,
            glyphLighting = false,
        )
    }
}

sealed interface AvatarGlyph {
    data object None : AvatarGlyph

    /** [sizeFraction] is the font size as a fraction of the squircle's side. */
    data class Letter(
        val text: String,
        val sizeFraction: Float = 0.58f,
        val typeface: Typeface = Typeface.DEFAULT_BOLD,
    ) : AvatarGlyph

    /** [pathData] is SVG path data inside a square of [viewport] units. [sizeFraction] is the icon box as a fraction of the squircle's side. */
    data class Icon(
        val pathData: String,
        val sizeFraction: Float = 0.43f,
        val viewport: Float = 100f,
    ) : AvatarGlyph

    companion object {
        val Bolt = Icon("M56 4 L16 56 H44 L38 96 L84 40 H54 Z")
        val Heart = Icon(
            "M50 90 C20 68 8 50 8 33 C8 19 19 10 30 10 C39 10 46 15 50 23 " +
                    "C54 15 61 10 70 10 C81 10 92 19 92 33 C92 50 80 68 50 90 Z"
        )
    }
}

/**
 * The squircle outline on a 52-unit grid, scaled to the given box:
 * M0 26C0 4.589 4.589 0 26 0C47.411 0 52 4.589 52 26C52 47.411 47.411 52 26 52C4.589 52 0 47.411 0 26Z
 */
fun Path.addSquircle(left: Float, top: Float, width: Float, height: Float) {
    val kx = width / 52f
    val ky = height / 52f
    fun x(v: Float) = left + v * kx
    fun y(v: Float) = top + v * ky
    moveTo(x(0f), y(26f))
    cubicTo(x(0f), y(4.589f), x(4.589f), y(0f), x(26f), y(0f))
    cubicTo(x(47.411f), y(0f), x(52f), y(4.589f), x(52f), y(26f))
    cubicTo(x(52f), y(47.411f), x(47.411f), y(52f), x(26f), y(52f))
    cubicTo(x(4.589f), y(52f), x(0f), y(47.411f), x(0f), y(26f))
    close()
}

/** For clipping or bordering any composable with the same outline. */
val SquircleShape: Shape = GenericShape { size, _ ->
    addSquircle(0f, 0f, size.width, size.height)
}

/**
 * [size] is the full footprint including room for the drop shadow.
 * The squircle itself is 78% of it, offset slightly upward, unless [fillBounds] is true.
 * Embedded avatars use [fillBounds] to retain their image bounds and badge alignment.
 * [baseColor] keeps the identity palette's actual color; [hue] remains a standalone fallback.
 */
@Composable
fun SquircleAvatar(
    glyph: AvatarGlyph,
    modifier: Modifier = Modifier,
    size: Dp = 96.dp,
    hue: Float = 205f,
    layers: AvatarLayers = AvatarLayers.All,
    fillBounds: Boolean = false,
    baseColor: Color = Color.Unspecified,
) {
    Box(
        modifier
            .size(size)
            .drawWithCache {
                val s = this.size.minDimension
                val side = if (fillBounds) s else s * 0.78f
                val left = (s - side) / 2f
                val top = if (fillBounds) 0f else s * 0.07f
                val cx = left + side / 2f
                val cy = top + side / 2f
                val u = side / 192f // 1 unit of the original 192-unit design
                val source = if (baseColor != Color.Unspecified) baseColor else
                    Color.hsl(((hue % 360f) + 360f) % 360f, 0.5f, 0.5f)
                val pal = DepthPalette(source)

                val shape = Path().apply { addSquircle(left, top, side, side) }

                val iconPath: Path? = (glyph as? AvatarGlyph.Icon)?.let { g ->
                    val k = g.sizeFraction * side / g.viewport
                    val box = g.viewport * k
                    Path().also { p ->
                        PathParser().parsePathString(g.pathData).toPath(p)
                        p.transform(Matrix().apply {
                            translate(cx - box / 2f, cy - box / 2f)
                            scale(k, k)
                        })
                    }
                }

                onDrawBehind {
                    if (layers.dropShadow) {
                        shadowed(shape, pal.flat.toArgb(), 4.5f * u, 3f * u, pal.shade.copy(alpha = 0.5f))
                        shadowed(shape, pal.flat.toArgb(), 18f * u, 14f * u, pal.shade.copy(alpha = 0.38f))
                    }

                    val baseBrush = if (layers.base) {
                        Brush.linearGradient(
                            0f to pal.light, 0.5f to pal.flat, 1f to pal.dark,
                            start = Offset(left + 0.15f * side, top),
                            end = Offset(left + 0.85f * side, top + side),
                        )
                    } else {
                        SolidColor(pal.flat)
                    }
                    drawPath(shape, baseBrush)

                    if (layers.innerShadow) {
                        clipPath(shape) {
                            val frame = Path.combine(
                                PathOperation.Difference,
                                Path().apply {
                                    addRect(Rect(left - side, top - side, left + 2f * side, top + 2f * side))
                                },
                                shape,
                            )
                            // The frame lies outside the squircle and is clipped away. Only its shadow, cast upward, remains.
                            shadowed(frame, android.graphics.Color.BLACK, 12f * u, -8f * u, pal.shade.copy(alpha = 0.55f))
                            drawPath(
                                shape,
                                Brush.verticalGradient(
                                    0f to pal.light.copy(alpha = 0f),
                                    0.8f to pal.light.copy(alpha = 0f),
                                    1f to pal.light.copy(alpha = 0.6f),
                                    startY = top,
                                    endY = top + side,
                                ),
                                style = Stroke(width = 5f * u),
                            )
                        }
                    }

                    if (layers.rim) {
                        clipPath(shape) {
                            drawPath(
                                shape,
                                Brush.linearGradient(
                                    0f to Color.White.copy(alpha = 0.75f),
                                    0.35f to Color.White.copy(alpha = 0f),
                                    1f to Color.White.copy(alpha = 0f),
                                    start = Offset(left + 0.25f * side, top),
                                    end = Offset(left + 0.5f * side, top + side),
                                ),
                                style = Stroke(width = 4f * u),
                            )
                        }
                    }

                    if (layers.specular) {
                        drawPath(
                            shape,
                            Brush.radialGradient(
                                colors = listOf(pal.tint.copy(alpha = pal.highlightAlpha), pal.tint.copy(alpha = 0f)),
                                center = Offset(left + 0.3f * side, top + 0.08f * side),
                                radius = 0.65f * side,
                            ),
                        )
                    }

                    when (glyph) {
                        AvatarGlyph.None -> Unit
                        is AvatarGlyph.Letter -> drawIntoCanvas { canvas ->
                            val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                                typeface = glyph.typeface
                                textSize = glyph.sizeFraction * side
                                textAlign = Paint.Align.CENTER
                            }
                            val b = android.graphics.Rect()
                            paint.getTextBounds(glyph.text, 0, glyph.text.length, b)
                            val baseline = cy - b.exactCenterY()
                            applyGlyphFill(paint, layers.glyphLighting, pal, baseline + b.top, baseline + b.bottom, u)
                            canvas.nativeCanvas.drawText(glyph.text, cx, baseline, paint)
                        }

                        is AvatarGlyph.Icon -> iconPath?.let { p ->
                            drawIntoCanvas { canvas ->
                                val bounds = p.getBounds()
                                val paint = Paint(Paint.ANTI_ALIAS_FLAG)
                                applyGlyphFill(paint, layers.glyphLighting, pal, bounds.top, bounds.bottom, u)
                                canvas.nativeCanvas.drawPath(p.asAndroidPath(), paint)
                            }
                        }
                    }
                }
            }
    )
}

private fun applyGlyphFill(paint: Paint, lighting: Boolean, pal: DepthPalette, top: Float, bottom: Float, u: Float) {
    if (lighting) {
        paint.shader = LinearGradient(
            0f, top, 0f, bottom,
            android.graphics.Color.WHITE, pal.tint.toArgb(),
            Shader.TileMode.CLAMP,
        )
        paint.setShadowLayer(4f * u, 0f, 3f * u, pal.shade.copy(alpha = 0.5f).toArgb())
    } else {
        paint.color = android.graphics.Color.WHITE
    }
}

/** Draws [path] filled with [fill] and a blurred shadow. [radius] is the platform blur radius, not a Gaussian sigma. */
private fun DrawScope.shadowed(path: Path, fill: Int, radius: Float, dy: Float, shadow: Color) {
    drawIntoCanvas { canvas ->
        val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = fill
            setShadowLayer(radius, 0f, dy, shadow.toArgb())
        }
        canvas.nativeCanvas.drawPath(path.asAndroidPath(), paint)
    }
}

@Preview(showBackground = true, backgroundColor = 0xFFE6E9EE)
@Composable
private fun SquircleAvatarPreview() {
    Row(Modifier.padding(16.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        SquircleAvatar(AvatarGlyph.Letter("B"), layers = AvatarLayers.Flat)
        SquircleAvatar(AvatarGlyph.Letter("B"))
        SquircleAvatar(AvatarGlyph.Bolt, hue = 32f)
        SquircleAvatar(AvatarGlyph.Heart, hue = 336f)
    }
}