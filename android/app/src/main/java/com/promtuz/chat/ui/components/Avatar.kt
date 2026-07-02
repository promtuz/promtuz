package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.adjustLight

const val AVATAR_RADIUS_RATIO = 2.875f;

@ExperimentalMaterial3Api
@Composable
fun Avatar(
    name: String,
    size: Dp = 52.dp,
    clipRatio: Float = AVATAR_RADIUS_RATIO
) {
    val clip = RoundedCornerShape(size / clipRatio)
    val fallbackChars = name.split(" ")
        .filter { it.isNotBlank() }
        .map { it.first() }
        .joinToString("")
    val interactionSource = remember { MutableInteractionSource() }

    Box(
        Modifier
            .size(size)
            .clip(clip)
            .background(MaterialTheme.colorScheme.surfaceContainerHigh.copy(0.75f))
            .clickable(
                enabled = true,
                interactionSource = interactionSource,
//                indication = ripple(
//                    color = adjustLight(
//                        MaterialTheme.colorScheme.background, 0.3f
//                    )
//                ),
            ) {

            }, contentAlignment = Alignment.Center
    ) {
        Text(
            fallbackChars,
            fontWeight = FontWeight.Bold,
            fontSize = (size.value / 2.6f).sp,
            color = MaterialTheme.colorScheme.onBackground.copy(0.85f)
        )
    }
}