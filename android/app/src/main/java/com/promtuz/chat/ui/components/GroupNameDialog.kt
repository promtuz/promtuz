package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.GroupWork

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
                AppTextField(value, onValueChange, Modifier.focusRequester(focus), label = "Group name",
                    enabled = !busy, isError = length > 64,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    keyboardActions = KeyboardActions(onDone = { if (valid) onConfirm() }),
                    suffix = { AppTextFieldCounter(length, 64) })
                GroupWorkFeedback(work)
            }
        },
        confirmButton = { TextButton(onClick = onConfirm, enabled = valid) { Text(confirmLabel) } },
        dismissButton = { TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") } },
    )
}
