package com.promtuz.chat.ui.components

import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.layout.Column
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ChatVM

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatTopBar(name: String, viewModel: ChatVM) {
    val back = LocalOnBackPressedDispatcherOwner.current
    val typing by viewModel.typing.collectAsState()
    TopAppBar(
        title = {
            Column {
                Text(name, style = MaterialTheme.typography.titleMedium)
                if (typing) Text(
                    "typing…",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
        },
        navigationIcon = {
            IconButton(onClick = { back?.onBackPressedDispatcher?.onBackPressed() }) {
                DrawableIcon(R.drawable.i_back_chevron)
            }
        },
    )
}
