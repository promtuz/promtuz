package com.promtuz.chat.ui.components

import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM

@Composable
fun HomeFab(appViewModel: AppVM) {
    val colors = MaterialTheme.colorScheme

    FloatingActionButton({
        appViewModel.navigator.push(Routes.Contacts)
    }) {
        DrawableIcon(
            R.drawable.i_comment_plus,
            desc = "Contacts",
            tint = colors.onPrimaryContainer,
        )
    }
}
