package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

/** Composer: a rounded input pill (grows to 6 lines) + accent send, over a blurred bar. */
@Composable
fun ChatBottomBar(viewModel: ChatVM, haze: HazeState) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val input by viewModel.input.collectAsState()
    val hazeStyle = chatBarHaze()

    Row(
        Modifier
            .fillMaxWidth()
            .navigationBarsPadding()
            .imePadding()
            .padding(horizontal = 10.dp, vertical = 8.dp),
        verticalAlignment = Alignment.Bottom,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Box(
            Modifier
                .weight(1f)
                .clip(RoundedCornerShape(24.dp))
                .hazeEffect(haze, hazeStyle)
//                .background(colors.surfaceContainerHigh.copy(alpha = 0.85f))
                .padding(horizontal = 16.dp, vertical = 13.dp),
            contentAlignment = Alignment.CenterStart,
        ) {
            BasicTextField(
                value = input,
                onValueChange = { viewModel.input.value = it },
                textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface),
                cursorBrush = SolidColor(chat.accent),
                maxLines = 6,
                modifier = Modifier.fillMaxWidth(),
                decorationBox = { inner ->
                    if (input.isEmpty()) Text(
                        "Message",
                        style = MaterialTheme.typography.bodyLarge,
                        color = colors.onSurfaceVariant,
                    )
                    inner()
                },
            )
        }
        FilledIconButton(
            onClick = viewModel::send,
            enabled = input.isNotBlank(),
            modifier = Modifier
                .size(48.dp)
                .hazeEffect(haze, hazeStyle),
            colors = IconButtonDefaults.filledIconButtonColors(containerColor = chat.accent),
        ) {
            DrawableIcon(R.drawable.i_send, Modifier.size(20.dp))
        }
    }
}
