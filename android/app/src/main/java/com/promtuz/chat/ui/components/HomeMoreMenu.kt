package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.navigation.LocalSearchMotion
import com.promtuz.chat.presentation.viewmodel.AppVM

@Composable
fun HomeMoreMenu(viewModel: AppVM, modifier: Modifier = Modifier, showSearch: Boolean = false) {
    val motion = LocalSearchMotion.current
    val groups = remember(viewModel, showSearch, motion) {
        buildList {
            if (showSearch) add(listOf(MenuAction("Search", R.drawable.oi_search) {
                motion.origin = null
                viewModel.navigator.push(Routes.Search)
            }))
            add(listOf(MenuAction("My Profile", R.drawable.oi_user_circle) { viewModel.navigator.push(Routes.Profile) }))
            add(listOf(MenuAction("Settings", R.drawable.oi_settings) { viewModel.navigator.push(Routes.Settings) }))
        }
    }

    AppDropMenu(
        iconSize = 24.dp,
        anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp)) },
        groups = groups,
        modifier = modifier,
    )
}
