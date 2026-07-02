package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MediumTopAppBar
import androidx.compose.material3.Scaffold
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarColors
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.TopAppBarScrollBehavior
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

@Composable
fun SimpleScreen(
    title: @Composable (() -> Unit),
    modifier: Modifier = Modifier,
    actions: @Composable RowScope.() -> Unit = {},
    topBarColors: TopAppBarColors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.background),
    topBarModifier: Modifier = Modifier,
    content: @Composable ((PaddingValues) -> Unit)
) {
    Scaffold(
        modifier.fillMaxSize(),
        topBar = {
            TopAppBar(
                title = title,
                modifier = topBarModifier,
                navigationIcon = { GoBackButton() },
                colors = topBarColors,
                actions = actions
            )
        },
        content = { content(it) }
    )
}