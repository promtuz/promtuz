package com.promtuz.chat.ui.theme

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.colorspace.ColorSpaces

/**
 * Lighting derived from the actual surface color. Changing Oklab L and scaling a/b is equivalent
 * to changing OKLCH lightness/chroma while holding hue fixed. No HSL palette is reconstructed.
 * Instances belong in remember/drawWithCache, never in the per-frame drawing block.
 */
internal class DepthPalette(
    base: Color,
    depth: Float = 0.06f,
    chroma: Float = 1f,
    val highlightAlpha: Float = 0.12f,
) {
    private val lab = base.convert(ColorSpaces.Oklab)
    val flat = if (chroma == 1f) base else tone(0f, chroma)
    val light = tone(depth, chroma)
    val dark = tone(-depth, chroma)
    val shade = tone(-depth * 3f, chroma)
    val tint = tone(depth * 2f, chroma * 0.45f)

    private fun tone(lightnessDelta: Float, chroma: Float): Color {
        val lightness = (lab.red + lightnessDelta).coerceIn(0f, 1f)
        fun candidate(scale: Float) = Color(
            red = lightness,
            green = lab.green * scale,
            blue = lab.blue * scale,
            alpha = lab.alpha,
            colorSpace = ColorSpaces.Oklab,
        ).convert(ColorSpaces.ExtendedSrgb)
        fun Color.inGamut() = red in -0.00001f..1.00001f &&
            green in -0.00001f..1.00001f && blue in -0.00001f..1.00001f

        var result = candidate(chroma)
        if (!result.inGamut()) {
            // Reduce only chroma when a lighter/darker stop leaves sRGB. Clamping RGB channels
            // individually would change its hue. Extended sRGB exposes those excursions.
            var low = 0f
            var high = chroma
            result = candidate(low)
            repeat(12) {
                val middle = (low + high) / 2f
                val trial = candidate(middle)
                if (trial.inGamut()) {
                    low = middle
                    result = trial
                } else high = middle
            }
        }
        return result.convert(ColorSpaces.Srgb)
    }
}
