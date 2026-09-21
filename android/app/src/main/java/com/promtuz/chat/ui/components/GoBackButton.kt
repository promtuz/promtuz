package com.promtuz.chat.ui.components

import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

@Composable
fun GoBackButton(modifier: Modifier = Modifier) {
    val backHandler = LocalOnBackPressedDispatcherOwner.current
    IconButton({
        backHandler?.onBackPressedDispatcher?.onBackPressed()
    }, modifier) {
        TopBarMorphIcon(
            MorphGlyph.Back,
            "Go Back",
            tint = MaterialTheme.colorScheme.onSurface,
        )
    }
}
