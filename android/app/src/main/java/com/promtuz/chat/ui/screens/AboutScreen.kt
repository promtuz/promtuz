package com.promtuz.chat.ui.screens

import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import com.promtuz.chat.ui.components.FlexibleScreen

@Composable
fun AboutScreen() {
    FlexibleScreen({ Text("About") }) { padding, scrollBehavior ->
        Text("About Screen")
    }
}