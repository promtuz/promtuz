package com.promtuz.chat.ui.theme

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.tooling.preview.*
import androidx.compose.ui.unit.*


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

/**
 * Content settles into the background on its way down to the composer, so a
 * floating bar sits over a faded edge instead of a hard cut. One straight ramp,
 * not [gradientScrim]'s stepped curve — stops only bend the falloff, and here
 * the whole point is that it reads as a single even fade.
 */
@Composable
fun bottomScrim(base: Color = MaterialTheme.colorScheme.background) =
    Brush.verticalGradient(listOf(Color.Transparent, base))

@Composable
fun transparentTopAppBar() = TopAppBarDefaults.topAppBarColors(
    containerColor = Color.Transparent,
    scrolledContainerColor = Color.Transparent
)


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