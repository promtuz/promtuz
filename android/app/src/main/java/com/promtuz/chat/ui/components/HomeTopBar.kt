package com.promtuz.chat.ui.components

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.theme.gradientScrim


@Composable
fun HomeTopBar(
    appViewModel: AppVM,
) {
    TopAppBar(
        modifier = Modifier.background(gradientScrim()),
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
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
                        onDoubleClick = {
                            appViewModel.refreshChats()
                        }
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
            HomeMoreMenu(appViewModel)
        })
}