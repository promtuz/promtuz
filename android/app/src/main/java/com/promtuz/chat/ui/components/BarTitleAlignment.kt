package com.promtuz.chat.ui.components

import android.graphics.Paint
import android.graphics.Rect
import android.graphics.Typeface
import androidx.compose.foundation.layout.offset
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFontFamilyResolver
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontSynthesis
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import kotlin.math.roundToInt

/**
 * Center the cap-to-baseline area, like vertical trim in a design tool. Keep the
 * full layout height so accents, descenders and title transitions aren't clipped.
 * Measure a stable capital rather than the title itself, so changing words or
 * selection counts doesn't move the baseline.
 */
@Composable
internal fun Modifier.centerBarTitle(style: TextStyle): Modifier {
    val face by LocalFontFamilyResolver.current.resolve(
        style.fontFamily,
        style.fontWeight ?: FontWeight.Normal,
        style.fontStyle ?: FontStyle.Normal,
        style.fontSynthesis ?: FontSynthesis.All,
    )
    val fontSize = with(LocalDensity.current) { style.fontSize.toPx() }
    val shift = remember(face, fontSize) {
        val bounds = Rect()
        val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            typeface = face as Typeface
            textSize = fontSize
            getTextBounds("H", 0, 1, bounds)
        }
        val metrics = paint.fontMetricsInt
        // The centered line box uses ascent/descent; its visible cap area uses
        // -capHeight/0. Move between those centers using the resolved font metrics.
        ((metrics.ascent + metrics.descent - bounds.top) / 2f).roundToInt()
    }
    // Keep this outside title animations: their outgoing/incoming alignment lines
    // are transient and must not feed back into the title's position.
    return offset { IntOffset(0, shift) }
}
