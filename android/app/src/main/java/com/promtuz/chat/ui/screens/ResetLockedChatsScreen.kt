package com.promtuz.chat.ui.screens

import android.text.format.DateUtils
import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.GroupedScreen
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.rememberAvatar
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
    val chats by app.chats.collectAsState()
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    var selected by remember { mutableStateOf<PrivateResetOption?>(null) }
    var busy by remember { mutableStateOf(false) }
    val options by remember {
        observeQuery(setOf("messages", "conversations", "contacts", "private_conversations")) {
            withContext(Dispatchers.IO) { uniffi.core.lockedConversationResetOptions() }
        }
    }.collectAsState(emptyList())
    val (hidden, locked) = options.partition { it.tag != null }
    GroupedScreen({ Text("Reset a locked chat") }) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(),
            verticalArrangement = Arrangement.spacedBy(12.dp)) {
            if (options.isEmpty()) item { Text("No locked chats", Modifier.padding(12.dp), color = Promtuz.colors.muted) }
            else item {
                Text(
                    if (hidden.isEmpty()) "Pick the chat whose code you forgot. Its history on this phone is cleared."
                    else "Pick the chat by its tag. Its history on this phone is cleared.",
                    Modifier.padding(horizontal = 16.dp), style = FootnoteStyle, color = Promtuz.colors.muted,
                )
            }
            if (hidden.isNotEmpty()) item {
                SettingsCard(header = "Hidden chats") {
                    hidden.forEach { option ->
                        key(option.conversationId.toHex()) {
                            SettingsRow("Hidden chat", subtitle = lockedAgo(option), trailing = Trailing.Chevron,
                                leading = { TagChip(option.tag!!) }, leadingSize = 40.dp,
                                onClick = { if (!busy) { error = null; selected = option } })
                        }
                    }
                }
            }
            if (locked.isNotEmpty()) item {
                SettingsCard(header = if (hidden.isEmpty()) null else "Locked chats") {
                    locked.forEach { option ->
                        val hex = option.conversationId.toHex()
                        key(hex) {
                            val chat = chats.firstOrNull { it.conversationHex == hex }
                            val picture = rememberAvatar(chat?.peerHex)
                            SettingsRow(option.name, subtitle = lockedAgo(option), trailing = Trailing.Chevron,
                                leading = {
                                    if (option.isGroup) GroupAvatar(option.name, emptyList(), 40.dp, conversation = hex)
                                    else Avatar(option.name, 40.dp, image = picture, identityKey = chat?.peerHex ?: hex)
                                }, leadingSize = 40.dp,
                                onClick = { if (!busy) { error = null; selected = option } })
                        }
                    }
                }
            }
        }
    }
    selected?.let { option ->
        val isHidden = option.tag != null
        AppAlertDialog(onDismissRequest = { if (!busy) selected = null },
            title = { Text(if (isHidden) "Reset hidden chat ${option.tag!!.code}?" else "Reset ${option.name}?") },
            text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(when {
                    isHidden -> "This deletes the chat and its history from this phone. You can add them again."
                    option.isGroup -> "This permanently deletes the locked history. You stay in the group. Another member must be available to restore messaging."
                    else -> "This permanently deletes the locked history. The contact stays, and you can start a fresh chat."
                })
                error?.let { Text(it, color = Promtuz.colors.danger) }
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
            }) { Text(if (isHidden) "Delete chat" else "Delete history", color = Promtuz.colors.danger) } },
            dismissButton = { TextButton(enabled = !busy, onClick = { selected = null }) { Text("Cancel") } })
    }
}

private fun lockedAgo(option: PrivateResetOption): String? = option.lockedAt.toLong().takeIf { it > 0 }?.let {
    "Locked " + DateUtils.getRelativeTimeSpanString(it * 1000, System.currentTimeMillis(), DateUtils.DAY_IN_MILLIS)
        .toString().replaceFirstChar { c -> c.lowercase() }
}
