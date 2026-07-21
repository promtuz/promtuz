package com.promtuz.chat.ui.components

import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.IconButton
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
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalFocusManager
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

/** Composer: one blurred pill holding the input (grows to 6 lines) and the accent send circle. */
@Composable
fun ChatBottomBar(viewModel: ChatVM, haze: HazeState) {
    val input by viewModel.input.collectAsState()
    val action by viewModel.composerAction.collectAsState()

    // The attach panel swaps with the keyboard, so its open-state and the system
    // pickers live here — both the paperclip toggle and the panel's tabs reach them.
    var attachOpen by remember { mutableStateOf(false) }
    val focusManager = LocalFocusManager.current

    // Permissionless system pickers (photo-picker / SAF), so no storage permission needed.
    val photoPicker = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia()) { uris ->
        if (uris.isNotEmpty()) { viewModel.attachPhotos(uris); attachOpen = false }
    }
    val filePicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isNotEmpty()) { viewModel.attachFiles(uris); attachOpen = false }
    }

    // Back closes the panel before the nav stack.
    BackHandler(attachOpen) { attachOpen = false }

    // No .imePadding()/.navigationBarsPadding(): AttachPanel owns the bottom region
    // and reserves the keyboard/nav space itself (see its region formula).
    Column(Modifier.fillMaxWidth()) {
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
        ComposerRow(
            viewModel, input, action, haze,
            attachOpen = attachOpen,
            onToggleAttach = {
                attachOpen = !attachOpen
                if (attachOpen) focusManager.clearFocus() // hide keyboard so the panel takes the region
            },
            onFieldFocused = { attachOpen = false },
        )
        AttachPanel(
            open = attachOpen,
            onPickPhotos = {
                photoPicker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageAndVideo))
            },
            onPickFiles = { filePicker.launch(arrayOf("*/*")) },
        )
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
private fun ComposerRow(
    viewModel: ChatVM,
    input: String,
    action: ComposerAction?,
    haze: HazeState,
    attachOpen: Boolean,
    onToggleAttach: () -> Unit,
    onFieldFocused: () -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current

    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 10.dp, vertical = 8.dp)
            .clip(RoundedCornerShape(26.dp))
            // Freeze must sit on the same chain as the blur it bakes (screen-space
            // Haze shatters under the exiting nav card's scale).
            .freezeOnExit()
            .hazeEffect(haze, chatBarHaze())
            .padding(start = 6.dp, end = 6.dp, top = 6.dp, bottom = 6.dp),
        verticalAlignment = Alignment.Bottom,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        // Paperclip toggles the attach panel (was a drop menu). Subtle rotate + accent tint when open.
        val attachRot by animateFloatAsState(if (attachOpen) 45f else 0f, tween(200), label = "attachRot")
        Box(
            Modifier.size(38.dp).clip(CircleShape).clickable(onClick = onToggleAttach),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(
                R.drawable.i_link,
                Modifier.size(22.dp).rotate(attachRot),
                tint = if (attachOpen) chat.accent else colors.onSurfaceVariant,
            )
        }
        BasicTextField(
            value = input,
            onValueChange = { viewModel.input.value = it },
            textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface),
            cursorBrush = SolidColor(chat.accent),
            maxLines = 6,
            // Tapping the field to type raises the keyboard, so close the panel it replaces.
            modifier = Modifier.weight(1f).padding(vertical = 7.dp)
                .onFocusChanged { if (it.isFocused) onFieldFocused() },
            decorationBox = { inner ->
                if (input.isEmpty()) Text(
                    "Message",
                    style = MaterialTheme.typography.bodyLarge,
                    color = colors.onSurfaceVariant,
                )
                inner()
            },
        )
        // The trailing slot is ALWAYS occupied at a fixed size so the pill's
        // height never jumps: mic by default (voice notes soon), send when
        // there's a draft, crossfading in place. Solid accent, no haze — a
        // blurred layer under the circle rendered as a square.
        val hasDraft = input.isNotBlank()
        Box(
            Modifier
                .size(38.dp)
                .clip(CircleShape)
                .background(if (hasDraft) chat.accent else Color.Transparent)
                .clickable(enabled = hasDraft) { viewModel.send() },
            contentAlignment = Alignment.Center,
        ) {
            AnimatedContent(
                targetState = when {
                    action is ComposerAction.Edit && hasDraft -> R.drawable.i_check
                    hasDraft -> R.drawable.i_send
                    else -> R.drawable.i_mic
                },
                transitionSpec = {
                    (scaleIn(tween(140), 0.6f) + fadeIn(tween(140)))
                        .togetherWith(scaleOut(tween(140), 0.6f) + fadeOut(tween(140)))
                },
                label = "composerAction",
            ) { icon ->
                DrawableIcon(
                    icon,
                    Modifier.size(18.dp),
                    tint = if (hasDraft) colors.onPrimary else colors.onSurfaceVariant,
                )
            }
        }
    }
}
