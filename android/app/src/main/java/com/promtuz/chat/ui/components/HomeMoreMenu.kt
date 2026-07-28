package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM

@Composable
fun HomeMoreMenu(viewModel: AppVM, modifier: Modifier = Modifier) {
    val groups = remember(viewModel) {
        listOf(
            listOf(MenuAction("My Profile", R.drawable.oi_user_circle) {}),
            listOf(MenuAction("Settings", R.drawable.oi_settings) { viewModel.navigator.push(Routes.Settings) }),
        )
    }

    AppDropMenu(
        iconSize = 20.dp,
        anchor = { DrawableIcon(R.drawable.i_ellipsis_vertical, Modifier.padding(12.dp)) },
        groups = groups,
        modifier = modifier,
    )
}
