package com.promtuz.chat.ui.components

import android.text.format.DateUtils
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatTopBar(name: String, viewModel: ChatVM, haze: HazeState) {
    val back = LocalOnBackPressedDispatcherOwner.current
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val typing by viewModel.typing.collectAsState()
    val presence by viewModel.presence.collectAsState()

    // Subtitle cascade: live activity beats presence; silence renders nothing.
    val (subtitle, subtitleColor) = when {
        typing -> "typing…" to chat.accent
        presence == Presence.Online -> "online" to chat.accent
        presence is Presence.LastSeen -> {
            val at = (presence as Presence.LastSeen).atMs
            val rel = DateUtils.getRelativeTimeSpanString(at, System.currentTimeMillis(), DateUtils.MINUTE_IN_MILLIS)
            "last seen $rel" to colors.onSurfaceVariant
        }
        else -> null to colors.onSurfaceVariant
    }

    TopAppBar(
        title = {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Avatar(name, 40.dp)
                Column {
                    Text(name, style = MaterialTheme.typography.titleMediumEmphasized, maxLines = 1)
                    if (subtitle != null) Text(
                        subtitle,
                        style = MaterialTheme.typography.labelMedium,
                        color = subtitleColor,
                    )
                }
            }
        },
        navigationIcon = {
            IconButton(onClick = { back?.onBackPressedDispatcher?.onBackPressed() }) {
                DrawableIcon(R.drawable.i_back_chevron)
            }
        },
        // freezeOnExit: bake the blur to pixels while the nav card scales out (Haze
        // samples screen-space and shatters under an ancestor scale).
        modifier = Modifier.freezeOnExit().hazeEffect(haze, chatBarHaze()),
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
    )
}
