package com.promtuz.chat.ui.components

import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Chat
import com.promtuz.chat.presentation.viewmodel.ChatVM
import dev.chrisbanes.haze.HazeDefaults
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.HazeStyle
import dev.chrisbanes.haze.hazeEffect

@Composable
fun ChatTopBar(chat: Chat, viewModel: ChatVM, haze: HazeState) {
    val backHandler = LocalOnBackPressedDispatcherOwner.current
    val colors = MaterialTheme.colorScheme
    val textStyle = MaterialTheme.typography
    val backIndicationSource = remember { MutableInteractionSource() }

    val chatBarColors = TopAppBarDefaults.topAppBarColors(
        colors.surfaceContainerLow.copy(0.1f),
        subtitleContentColor = colors.onSurfaceVariant.copy(0.65f)
    )

    val hazeStyle = HazeStyle(
        colors.surfaceContainerLow,
        HazeDefaults.tint(colors.surfaceContainerLow),
        48.dp,
        0.1f,
        HazeDefaults.tint(colors.surfaceContainerLow)
    )

    TopAppBar(
        title = {
            Text(chat.nickname, style = textStyle.titleMediumEmphasized.copy(fontSize = 18.sp))
        },
        subtitle = {
            Text("Last seen 2 min ago")
        },
        modifier = Modifier
            .fillMaxWidth()
            .hazeEffect(haze, hazeStyle),
        navigationIcon = {
            Row(
                Modifier.padding(6.dp, 0.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(4.dp)
            ) {
                DrawableIcon(
                    R.drawable.i_back_chevron, Modifier.clickable(
                        interactionSource = backIndicationSource,
                        indication = null // no ripple
                    ) {
                        backHandler?.onBackPressedDispatcher?.onBackPressed()
                    })
                Avatar(chat.nickname, 42.dp)
            }
        },
        actions = {
            IconButton({}) {
                DrawableIcon(R.drawable.i_ellipsis_vertical)
            }
        },
        colors = chatBarColors,
    )
}
