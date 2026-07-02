package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.res.*

@Composable
fun DrawableIcon(
    @DrawableRes id: Int,
    modifier: Modifier = Modifier,
    desc: String = "",
    tint: Color = MaterialTheme.colorScheme.onSurface
) {
    Icon(
        painter = painterResource(id),
        desc,
        modifier,
        tint
    )
}