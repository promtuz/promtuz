package com.promtuz.chat.ui.screens

import androidx.activity.compose.BackHandler
import com.promtuz.chat.ui.components.*
import androidx.compose.foundation.lazy.LazyColumn
import com.promtuz.chat.navigation.LocalNavForeground

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.launch

@Composable
fun LockChatScreen(conversation: String, onBack: () -> Unit, onLocked: () -> Unit) {
    DisposableEffect(Unit) {
        val protection = com.promtuz.chat.security.PrivateChatSession.protectInput()
        onDispose { protection.close() }
    }
    var code by remember { mutableStateOf("") }
    var visible by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var pendingCode by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    val context = androidx.compose.ui.platform.LocalContext.current
    val foreground = LocalNavForeground.current
    BackHandler(busy) { }
    fun requestLock() {
        if (foreground && !busy && code.length >= 6) pendingCode = code
    }
    fun lock(chosenCode: String) {
        if (!foreground || busy || chosenCode.length < 6) return
        busy = true
        scope.launch {
            runCatching { CoreBridge.hideConversation(conversation.fromHex(), chosenCode) }
                .onSuccess {
                    com.promtuz.core.call.CallNotifications.clearConversation(context, conversation.fromHex())
                    runCatching { com.promtuz.core.push.PushNotifier.refresh() }
                    code = ""
                    onLocked()
                }
                .onFailure { error = it.message ?: "Couldn't lock this chat" }
            busy = false
        }
    }
    SimpleScreen(
        title = { Text("Lock chat") },
        connectionStatus = false,
        navigationIcon = {
            IconButton(onClick = { if (!busy) onBack() }, enabled = !busy) {
                TopBarMorphIcon(MorphGlyph.Back, "Back")
            }
        },
        actions = {
            IconButton(enabled = foreground && !busy && code.length >= 6, onClick = { requestLock() }) {
                if (busy) CircularProgressIndicator(
                    Modifier.size(24.dp), strokeWidth = 2.dp,
                    color = MaterialTheme.colorScheme.primary,
                ) else DrawableIcon(R.drawable.oi_lock_chat, desc = "Lock chat", tint = LocalContentColor.current)
            }
        },
    ) { padding ->
        LazyColumn(Modifier.fillMaxSize().consumeWindowInsets(padding).imePadding(),
            contentPadding = padding.listPadding(top = 16.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp)) {
            item {
                AppTextField(
                    value = code,
                    onValueChange = { code = it; error = null },
                    label = "Code",
                    placeholder = "6+ characters",
                    enabled = !busy,
                    isError = error != null,
                    visualTransformation = if (visible) VisualTransformation.None else PasswordVisualTransformation(),
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Password,
                        autoCorrectEnabled = false,
                        imeAction = ImeAction.Done,
                    ),
                    keyboardActions = KeyboardActions(onDone = { requestLock() }),
                    trailingIcon = {
                        IconButton(enabled = !busy, onClick = { visible = !visible }) {
                            DrawableIcon(
                                if (visible) R.drawable.oi_eye_slash else R.drawable.oi_eye,
                                desc = if (visible) "Hide code" else "Show code",
                                tint = LocalContentColor.current,
                            )
                        }
                    },
                    supportingIcon = { DrawableIcon(R.drawable.oi_info, size = 16.dp, tint = LocalContentColor.current) },
                    supportingText = {
                        Text(error ?: "Enter this code in home search to open the chat.")
                    },
                )
            }
            item {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Hidden and silent", style = MaterialTheme.typography.titleMedium)
                    Text("Hidden from chat lists and normal search. Message notifications are disabled for this chat.",
                        style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            item {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Keep your code", style = MaterialTheme.typography.titleMedium)
                    Text("Your code protects this chat’s messages and media. If you forget it, resetting the chat deletes its history. The contact stays.",
                        style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
    }
    pendingCode?.let { chosenCode ->
        AppAlertDialog(
            onDismissRequest = { pendingCode = null },
            title = { Text("Lock this chat?") },
            text = { Text("This hides the chat and turns off its message notifications. If you forget the code, you’ll need to delete all its history to use it again. The contact stays.") },
            confirmButton = {
                TextButton(enabled = foreground && !busy, onClick = {
                    pendingCode = null
                    lock(chosenCode)
                }) { Text("Lock chat") }
            },
            dismissButton = {
                TextButton(onClick = { pendingCode = null }) { Text("Cancel") }
            },
        )
    }
}
