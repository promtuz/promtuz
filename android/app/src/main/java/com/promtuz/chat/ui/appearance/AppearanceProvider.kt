package com.promtuz.chat.ui.appearance

import androidx.compose.runtime.ProvidableCompositionLocal
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * The active chat look, read by every chat renderer. Mounted by PromtuzTheme from
 * [AppearanceStore]. Non-static local: editor sliders mutate this per drag-frame,
 * so only actual readers recompose.
 */
val LocalChatAppearance: ProvidableCompositionLocal<ChatAppearance> =
    compositionLocalOf { ChatAppearance.Default }

// ── token → Compose conversions (the primitive-to-Compose boundary) ────────────

val BubbleStyle.corner: Dp get() = cornerRadius.dp
val BubbleStyle.nearCorner: Dp get() = nearCornerRadius.dp
val LayoutStyle.messageGapDp: Dp get() = messageGap.dp
val LayoutStyle.groupGapDp: Dp get() = groupGap.dp
