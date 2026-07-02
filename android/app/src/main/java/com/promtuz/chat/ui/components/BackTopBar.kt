package com.promtuz.chat.ui.components

import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.*
import com.promtuz.chat.ui.text.avgSizeInStyle


@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun BackTopBar(
    title: String
) {
    val textTheme = MaterialTheme.typography

    CenterAlignedTopAppBar(
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
        navigationIcon = { GoBackButton() }, title = {
            Text(
                title, style = avgSizeInStyle(
                    textTheme.titleLargeEmphasized, textTheme.titleMediumEmphasized
                )
            )
        })
}