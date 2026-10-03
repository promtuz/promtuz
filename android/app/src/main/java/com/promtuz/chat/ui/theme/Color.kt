package com.promtuz.chat.ui.theme

import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.*


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

/** One straight ramp, not [gradientScrim]'s stepped curve, so it reads as a single even fade. */
@Composable
fun bottomScrim(base: Color = MaterialTheme.colorScheme.background) =
    Brush.verticalGradient(listOf(Color.Transparent, base))

@Composable
fun transparentTopAppBar() = TopAppBarDefaults.topAppBarColors(
    containerColor = Color.Transparent,
    scrolledContainerColor = Color.Transparent
)
