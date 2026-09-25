package com.promtuz.chat.ui.components

import androidx.compose.material3.Text
import androidx.compose.runtime.Composable

@Composable
fun BackTopBar(title: String) {
    AppTopBar(title = { Text(title) })
}
