package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.launch

@Composable
fun LockChatDialog(conversation: String, onDismiss: () -> Unit, onLocked: () -> Unit) {
    DisposableEffect(Unit) {
        val protection = com.promtuz.chat.security.PrivateChatSession.protectInput()
        onDispose { protection.close() }
    }
    var code by remember { mutableStateOf("") }
    var repeated by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    val context = androidx.compose.ui.platform.LocalContext.current
    AppAlertDialog(onDismissRequest = { if (!busy) onDismiss() }, title = { Text("Lock chat") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("Enter this code in home search to open the chat. Notifications will be off. If you forget the code, you can reset the chat, but its history will be lost.")
            OutlinedTextField(code, { code = it; error = null }, label = { Text("Code (at least 6 characters)") }, singleLine = true,
                enabled = !busy, visualTransformation = PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false))
            OutlinedTextField(repeated, { repeated = it; error = null }, label = { Text("Repeat code") }, singleLine = true,
                enabled = !busy, visualTransformation = PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false))
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        } },
        confirmButton = { TextButton(enabled = !busy && code.length >= 6 && code == repeated, onClick = {
            busy = true
            scope.launch {
                runCatching { CoreBridge.hideConversation(conversation.fromHex(), code) }
                     .onSuccess {
                        com.promtuz.core.call.CallNotifications.clearConversation(context, conversation.fromHex())
                        runCatching { com.promtuz.core.push.PushNotifier.refresh() }
                        code = ""; repeated = ""; onLocked()
                    }
                    .onFailure { error = it.message ?: "Couldn't lock this chat" }
                busy = false
            }
        }) { Text("Lock") } },
        dismissButton = { TextButton(enabled = !busy, onClick = onDismiss) { Text("Cancel") } })
}
