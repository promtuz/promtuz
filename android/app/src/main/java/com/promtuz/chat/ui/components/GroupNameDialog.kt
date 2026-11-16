package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.animation.*
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.ui.theme.Radii

@Composable
fun GroupNameDialog(
    visible: Boolean,
    heading: String,
    value: String,
    onValueChange: (String) -> Unit,
    work: GroupWork,
    confirmLabel: String,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
    changed: Boolean = true,
    summary: String? = null,
) {
    val busy = work is GroupWork.Busy
    val length = value.codePointCount(0, value.length)
    val valid = visible && value.isNotBlank() && length <= 64 && changed && !busy
    if (visible) AppAlertDialog(onDismissRequest = { if (!busy) onDismiss() },
        title = { Text(heading) },
        text = {
            val focus = remember { FocusRequester() }
            LaunchedEffect(Unit) { focus.requestFocus() }
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                summary?.let { Text(it, color = Promtuz.colors.muted, maxLines = 3) }
                BasicTextField(value, onValueChange, enabled = !busy, singleLine = true,
                    textStyle = MaterialTheme.typography.titleMedium.copy(color = Promtuz.colors.text),
                    cursorBrush = SolidColor(Promtuz.colors.accentFill),
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    keyboardActions = KeyboardActions(onDone = { if (valid) onConfirm() }),
                    modifier = Modifier.fillMaxWidth().focusRequester(focus).semantics { contentDescription = "Group name" }
                        .clip(RoundedCornerShape(Radii.field)).background(cardFill()).padding(horizontal = 18.dp, vertical = 18.dp),
                    decorationBox = { inner -> Box {
                        if (value.isEmpty()) Text("Group name", color = Promtuz.colors.muted,
                            style = MaterialTheme.typography.titleMedium)
                        inner()
                    } })
                Text("$length/64", Modifier.align(Alignment.End), style = MaterialTheme.typography.labelSmall,
                    color = if (length > 64) Promtuz.colors.danger else Promtuz.colors.muted)
                GroupWorkFeedback(work)
            }
        },
        confirmButton = { TextButton(onClick = onConfirm, enabled = valid) { Text(confirmLabel) } },
        dismissButton = { TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") } },
    )
}
