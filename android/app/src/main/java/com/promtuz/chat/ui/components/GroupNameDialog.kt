package com.promtuz.chat.ui.components

import androidx.compose.animation.*
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
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
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.ui.stage.ChatMotion

@Composable
fun GroupActionButton(label: String, onClick: () -> Unit, enabled: Boolean = true, modifier: Modifier = Modifier) {
    val colors = MaterialTheme.colorScheme
    val fill by animateColorAsState(if (enabled) colors.primary else colors.surfaceContainerHigh,
        ChatMotion.spec(), label = "group action fill")
    Box(modifier.clip(CircleShape).background(fill).clickable(enabled = enabled, role = Role.Button, onClick = onClick)
        .padding(horizontal = 24.dp, vertical = 14.dp), contentAlignment = Alignment.Center) {
        Text(label, color = if (enabled) colors.onPrimary else colors.onSurfaceVariant,
            style = MaterialTheme.typography.labelLarge, fontWeight = FontWeight.SemiBold)
    }
}

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
            val colors = MaterialTheme.colorScheme
            val focus = remember { FocusRequester() }
            LaunchedEffect(Unit) { focus.requestFocus() }
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                summary?.let { Text(it, color = colors.onSurfaceVariant, maxLines = 3) }
                BasicTextField(value, onValueChange, enabled = !busy, singleLine = true,
                    textStyle = MaterialTheme.typography.titleMedium.copy(color = colors.onSurface),
                    cursorBrush = SolidColor(colors.primary),
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    keyboardActions = KeyboardActions(onDone = { if (valid) onConfirm() }),
                    modifier = Modifier.fillMaxWidth().focusRequester(focus).semantics { contentDescription = "Group name" }
                        .clip(RoundedCornerShape(24.dp)).background(colors.surfaceContainerHigh).padding(horizontal = 18.dp, vertical = 18.dp),
                    decorationBox = { inner -> Box {
                        if (value.isEmpty()) Text("Group name", color = colors.onSurfaceVariant,
                            style = MaterialTheme.typography.titleMedium)
                        inner()
                    } })
                Text("$length/64", Modifier.align(Alignment.End), style = MaterialTheme.typography.labelSmall,
                    color = if (length > 64) colors.error else colors.onSurfaceVariant)
                GroupWorkFeedback(work)
            }
        },
        confirmButton = { GroupActionButton(confirmLabel, onConfirm, valid) },
        dismissButton = { TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") } },
    )
}
