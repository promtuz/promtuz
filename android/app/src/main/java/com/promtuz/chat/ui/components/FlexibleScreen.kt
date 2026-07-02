package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*


@Composable
fun FlexibleScreen(
    title: @Composable (() -> Unit),
    modifier: Modifier = Modifier,
    scrollBehavior: TopAppBarScrollBehavior = TopAppBarDefaults.exitUntilCollapsedScrollBehavior(),
    topBarColors: TopAppBarColors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.background),
    topBarModifier: Modifier = Modifier,
    content: @Composable ((PaddingValues, TopAppBarScrollBehavior) -> Unit)
) {
    Scaffold(
        modifier.fillMaxSize(),
        topBar = {
            MediumFlexibleTopAppBar(
                title = title,
                modifier = topBarModifier,
                navigationIcon = { GoBackButton() },
                colors = topBarColors,
                scrollBehavior = scrollBehavior
            )
        },
        content = { content(it, scrollBehavior) }
    )
}