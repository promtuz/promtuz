package com.promtuz.chat.ui.theme

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.tooling.preview.*
import androidx.compose.ui.unit.*
import androidx.core.graphics.ColorUtils


@Composable
fun gradientScrim(base: Color = MaterialTheme.colorScheme.background) = Brush.verticalGradient(
    listOf(
        base.copy(alpha = 0.95f),
        base.copy(alpha = 0.9f),
        base.copy(alpha = 0.8f),
        base.copy(alpha = 0.65f),
        base.copy(alpha = 0.5f),
        base.copy(alpha = 0.2f),
        Color.Transparent
    )
)

@Composable
fun transparentTopAppBar() = TopAppBarDefaults.topAppBarColors(
    containerColor = Color.Transparent,
    scrolledContainerColor = Color.Transparent
)


/**
 *
 * 1f changeInLight is 100%
 *
 * Example:
 * 1. `+0.3f` will increase light by `30%` regardless of base value, but will max out at 100% (pure white)
 * 2. `-0.3f` will decrease light by `30%` regardless of base value, but will max out at 0% (pitch black)
 *
 */
fun adjustLight(col: Color, changeInLight: Float): Color {
    val hsl = floatArrayOf(0f, 0f, 0f)
    ColorUtils.RGBToHSL(
        (col.red * 255f).toInt(),
        (col.green * 255f).toInt(),
        (col.blue * 255f).toInt(),
        hsl
    )
    hsl[2] += changeInLight
    return Color(ColorUtils.HSLToColor(hsl))
}

@Preview(wallpaper = Wallpapers.BLUE_DOMINATED_EXAMPLE)
@Composable
private fun SurfaceColorsPreview(modifier: Modifier = Modifier) {
    PromtuzTheme(true) {
        val colors = MaterialTheme.colorScheme

        Row(Modifier.fillMaxWidth()) {
            ColoredBox(colors.primary, "PRIMARY")
            ColoredBox(colors.secondary, "SECONDARY")
            ColoredBox(colors.tertiary, "TERTIARY")
        }
    }
}


@Composable
private fun RowScope.ColoredBox(col: Color, label: String) {
    Box(
        Modifier
            .weight(1f)
            .background(col)
            .padding(vertical = 18.dp),
        contentAlignment = Alignment.Center
    ) {
        Text(text = label, style = MaterialTheme.typography.labelMediumEmphasized)
    }
}