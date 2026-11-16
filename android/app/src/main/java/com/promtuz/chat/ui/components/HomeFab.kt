package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.FloatingActionButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Paint
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.theme.DepthPalette

@Composable
fun HomeFab(appViewModel: AppVM) {
    val primary = Promtuz.colors.accentFill
    val palette = remember(primary) { DepthPalette(primary) }
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale = animateFloatAsState(if (pressed) 0.96f else 1f,
        animationSpec = Tweens.microInteraction(140), label = "new chat press")
    FloatingActionButton(
        onClick = { appViewModel.navigator.push(Routes.Contacts) },
        modifier = Modifier.graphicsLayer { scaleX = scale.value; scaleY = scale.value },
        shape = SquircleShape,
        containerColor = palette.flat,
        contentColor = Color.White,
        elevation = FloatingActionButtonDefaults.elevation(defaultElevation = 8.dp, pressedElevation = 4.dp),
        interactionSource = interaction,
    ) {
        Box(Modifier.size(56.dp).drawWithCache {
            val base = Brush.linearGradient(
                0f to palette.light, 0.5f to palette.flat, 1f to palette.dark,
                start = Offset.Zero, end = Offset(size.width, size.height),
            )
            val highlight = Brush.radialGradient(
                listOf(palette.tint.copy(alpha = palette.highlightAlpha), palette.tint.copy(alpha = 0f)),
                center = Offset(size.width * 0.25f, size.height * 0.05f),
                radius = size.width * 0.8f,
            )
            onDrawBehind {
                drawRect(base)
                drawRect(highlight)
            }
        }, contentAlignment = Alignment.Center) {
            DrawableIcon(R.drawable.i_comment_plus, desc = "New chat", tint = Color.White,
                modifier = Modifier.glyphLighting(palette))
        }
    }
}

/** Apply the avatar glyph's light-to-tinted fill to the existing drawable's alpha mask. */
private fun Modifier.glyphLighting(palette: DepthPalette) = drawWithCache {
    val fill = Brush.verticalGradient(listOf(Color.White, lerp(Color.White, palette.tint, 0.16f)))
    val shadow = SolidColor(palette.shade.copy(alpha = 0.45f))
    val dy = 0.75.dp.toPx()
    val margin = 2.dp.toPx()
    val bounds = Rect(-margin, -margin, size.width + margin, size.height + margin)
    val paint = Paint()
    onDrawWithContent {
        val canvas = drawContext.canvas
        canvas.saveLayer(bounds, paint)
        translate(top = dy) { this@onDrawWithContent.drawContent() }
        drawRect(shadow, topLeft = bounds.topLeft, size = bounds.size, blendMode = BlendMode.SrcIn)
        canvas.restore()
        canvas.saveLayer(bounds, paint)
        drawContent()
        drawRect(fill, topLeft = bounds.topLeft, size = bounds.size, blendMode = BlendMode.SrcIn)
        canvas.restore()
    }
}
