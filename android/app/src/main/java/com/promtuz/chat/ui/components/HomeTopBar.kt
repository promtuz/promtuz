package com.promtuz.chat.ui.components

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.theme.gradientScrim
import com.promtuz.chat.ui.theme.transparentTopAppBar


@Composable
fun HomeTopBar(
    appViewModel: AppVM,
) {
    // Home deliberately keeps its gradient over the scrolling chat list.
    TopAppBar(
        modifier = Modifier.background(gradientScrim()),
        colors = transparentTopAppBar(),
        navigationIcon = {
            Image(
                painterResource(R.drawable.logo_colored),
                contentDescription = "Promtuz App Logo",
                modifier = Modifier
                    .padding(horizontal = 12.dp)
                    .width(32.dp)
                    .combinedClickable(
                        indication = null,
                        interactionSource = null,
                        onClick = {},
                        onDoubleClick = {}
                    )
            )
        },
        title = {
            AppBarDynamicTitle(
                appViewModel.dynamicTitle,
                Modifier.combinedClickable(
                    enabled = true,
                    interactionSource = null,
                    indication = null,
                    onClick = {},
                    onLongClick = {
                        appViewModel.navigator.push(Routes.Logs)
                    })
            )
        },
        actions = {
            AppUpdateIcon(onClick = { appViewModel.navigator.push(Routes.Updates) })
            HomeMoreMenu(appViewModel)
        })
}
