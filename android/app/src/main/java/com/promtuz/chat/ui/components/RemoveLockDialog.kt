package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.unit.dp
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.reason
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.launch

@Composable
fun RemoveLockDialog(app: AppVM, conversationHex: String, name: String, onDismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    AppAlertDialog(
        onDismissRequest = { if (!busy) onDismiss() },
        title = { Text("Remove lock?") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("This chat will open without a code and can notify you again.")
            error?.let { Text(it, color = Promtuz.colors.danger) }
        } },
        confirmButton = { TextButton(enabled = !busy, onClick = {
            busy = true
            scope.launch {
                runCatching { CoreBridge.removeConversationLock(conversationHex.fromHex()) }
                    .onSuccess {
                        // The chat's entry holds a private session; swap it for an ordinary one.
                        val stack = app.navigator.backStack
                        val at = stack.indexOfLast { (it as? Routes.Chat)?.conversation == conversationHex }
                        if (at >= 0) stack[at] = Routes.Chat(conversationHex, name)
                        PrivateChatSession.clear()
                        app.clearPrivateSummary()
                        onDismiss()
                    }
                    .onFailure { error = it.reason("Couldn't remove the lock") }
                busy = false
            }
        }) { Text("Remove lock") } },
        dismissButton = { TextButton(enabled = !busy, onClick = onDismiss) { Text("Cancel") } },
    )
}
