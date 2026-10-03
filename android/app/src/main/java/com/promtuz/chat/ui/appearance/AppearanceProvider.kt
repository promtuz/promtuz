package com.promtuz.chat.ui.appearance

import androidx.compose.runtime.ProvidableCompositionLocal
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/** Non-static: editor sliders change it every drag frame, so only its readers should recompose. */
val LocalChatAppearance: ProvidableCompositionLocal<ChatAppearance> =
    compositionLocalOf { ChatAppearance.Default }

val BubbleStyle.corner: Dp get() = cornerRadius.dp
val BubbleStyle.nearCorner: Dp get() = nearCornerRadius.dp
val LayoutStyle.messageGapDp: Dp get() = messageGap.dp
val LayoutStyle.groupGapDp: Dp get() = groupGap.dp
