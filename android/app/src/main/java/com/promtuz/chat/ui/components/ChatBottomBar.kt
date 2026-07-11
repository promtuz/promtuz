package com.promtuz.chat.ui.components

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.ComposerAction
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

/** Composer: a rounded input pill (grows to 6 lines) + accent send, over a blurred bar. */
@Composable
fun ChatBottomBar(viewModel: ChatVM, haze: HazeState) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val input by viewModel.input.collectAsState()
    val action by viewModel.composerAction.collectAsState()
    val hazeStyle = chatBarHaze()

    Column(
        Modifier
            .fillMaxWidth()
            // Bake blur to pixels while the nav card scales out (screen-space Haze
            // shatters under an ancestor scale).
            .freezeOnExit()
            .navigationBarsPadding()
            .imePadding(),
    ) {
        // Chip content is captured (not read live) so the exit animation has
        // something to draw after the action nulls.
        var lastAction by remember { mutableStateOf(action) }
        if (action != null) lastAction = action
        AnimatedVisibility(
            action != null,
            enter = expandVertically(expandFrom = Alignment.Top) + fadeIn(),
            exit = shrinkVertically(shrinkTowards = Alignment.Top) + fadeOut(),
        ) {
            lastAction?.let { ComposerActionChip(it, viewModel::cancelComposerAction) }
        }
        ComposerRow(viewModel, input, action, haze)
    }
}

/** The staged reply/edit banner: accent rail + label + one-line snippet + cancel. */
@Composable
private fun ComposerActionChip(action: ComposerAction, onCancel: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val label = if (action is ComposerAction.Edit) "Edit message" else "Reply to"
    val snippet = if (action.msg.deleted) "Deleted message"
    else (action.msg.content as? MessageContent.Text)?.text.orEmpty()

    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 10.dp)
            .clip(RoundedCornerShape(14.dp))
            .background(colors.surfaceContainerHigh.copy(alpha = 0.92f))
            .padding(start = 12.dp, top = 8.dp, bottom = 8.dp, end = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .width(3.dp)
                .height(32.dp)
                .clip(RoundedCornerShape(2.dp))
                .background(chat.accent),
        )
        Column(Modifier.weight(1f).padding(horizontal = 10.dp)) {
            Text(label, style = MaterialTheme.typography.labelMedium, color = chat.accent)
            Text(
                snippet,
                style = MaterialTheme.typography.bodyMedium,
                color = colors.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        IconButton(onCancel) { DrawableIcon(R.drawable.i_close, Modifier.size(18.dp)) }
    }
}

@Composable
private fun ComposerRow(viewModel: ChatVM, input: String, action: ComposerAction?, haze: HazeState) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val hazeStyle = chatBarHaze()
    Row(
        Modifier
            .fillMaxWidth()
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
            val icon = if (action is ComposerAction.Edit) R.drawable.i_check else R.drawable.i_send
            DrawableIcon(icon, Modifier.size(20.dp))
        }
    }
}
