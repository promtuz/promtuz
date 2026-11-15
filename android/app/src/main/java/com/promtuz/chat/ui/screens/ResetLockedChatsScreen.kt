package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.GroupedScreen
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.koin.compose.koinInject
import uniffi.core.PrivateResetOption

/** Deliberately exposes identity only, through an explicit destructive settings action. */
@Composable
fun ResetLockedChatsScreen() {
    val app = koinInject<AppVM>()
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    var selected by remember { mutableStateOf<PrivateResetOption?>(null) }
    var busy by remember { mutableStateOf(false) }
    val options by remember {
        observeQuery(setOf("messages", "conversations", "contacts", "private_conversations")) {
            withContext(Dispatchers.IO) { uniffi.core.lockedConversationResetOptions() }
        }
    }.collectAsState(emptyList())
    GroupedScreen({ Text("Reset locked chat") }) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(),
            verticalArrangement = Arrangement.Top) {
            if (options.isEmpty()) item { Text("No locked chats", Modifier.padding(12.dp),
                color = MaterialTheme.colorScheme.onSurfaceVariant) }
            itemsIndexed(options, key = { _, option -> option.conversationId.toHex() }) { index, option ->
                GroupedActionRow(option.name, index, options.size,
                    onClick = { if (!busy) { error = null; selected = option } })
            }
        }
    }
    selected?.let { option ->
        AppAlertDialog(onDismissRequest = { if (!busy) selected = null },
            title = { Text("Reset ${option.name}?") },
            text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(if (option.isGroup)
                    "This permanently deletes the locked history. You stay in the group. Another member must be available to restore messaging."
                else "This permanently deletes the locked history. The contact stays, and you can start a fresh chat.")
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            } },
            confirmButton = { TextButton(enabled = !busy, onClick = {
                busy = true
                scope.launch {
                    runCatching { CoreBridge.resetPrivateConversation(option.conversationId) }
                        .onSuccess {
                            PrivateChatSession.clear()
                            app.clearPrivateSummary()
                            selected = null
                        }.onFailure { error = "Couldn't reset this chat" }
                    busy = false
                }
            }) { Text("Delete history", color = MaterialTheme.colorScheme.error) } },
            dismissButton = { TextButton(enabled = !busy, onClick = { selected = null }) { Text("Cancel") } })
    }
}
