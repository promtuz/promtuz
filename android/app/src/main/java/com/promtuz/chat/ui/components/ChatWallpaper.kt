package com.promtuz.chat.ui.components

import androidx.appcompat.content.res.AppCompatResources
import androidx.compose.foundation.background
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageShader
import androidx.compose.ui.graphics.ShaderBrush
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.core.graphics.drawable.toBitmap
import com.promtuz.chat.R
import com.promtuz.chat.ui.appearance.Wallpaper

/**
 * A Modifier that paints the chat [wallpaper] — a tiled pattern (built once, the
 * heavy bitmap remembered so it never rebuilds per frame) or a solid fill. Tint +
 * alpha ride outside the remembered brush so recolouring stays live.
 */
@Composable
fun rememberChatWallpaper(wallpaper: Wallpaper): Modifier {
    val colors = MaterialTheme.colorScheme
    return when (wallpaper) {
        is Wallpaper.Solid -> Modifier.background(Color(wallpaper.argb))
        is Wallpaper.Pattern -> {
            val ctx = LocalContext.current
            val brush = remember {
                val bmp = AppCompatResources.getDrawable(ctx, R.drawable.pattern_chat_topography)!!
                    .toBitmap(1200, 1200)
                    .asImageBitmap()
                ShaderBrush(ImageShader(bmp, TileMode.Repeated, TileMode.Repeated))
            }
            val base = wallpaper.tintArgb?.let { Color(it) } ?: colors.surface
            Modifier
                .background(base)
                .background(brush, alpha = wallpaper.alpha)
        }
    }
}
