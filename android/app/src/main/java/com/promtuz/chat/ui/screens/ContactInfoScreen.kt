package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.AppButton
import com.promtuz.chat.ui.components.ButtonKind
import com.promtuz.chat.ui.components.AppAlertDialog
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.media.*
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.core.CoreBridge
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.core.observeQuery
import kotlinx.coroutines.launch
import org.koin.compose.koinInject

@Composable
fun ContactInfoScreen(conversation: String?, peer: String, fallbackName: String = "") {
    val app = koinInject<AppVM>()
    val scope = rememberCoroutineScope()
    val profile by remember(peer) {
        observeQuery(setOf("contacts", "peer_profiles", "prefs")) { CoreBridge.personProfile(peer.fromHex()) }
    }.collectAsState(null)
    val chats by app.chats.collectAsState()
    val privateIds by app.privateIds.collectAsState()
    val privateSummary by app.privateSummary.collectAsState()
    // A Secret chat's info is its own; anywhere else it is the normal chat's.
    val chat = chats.firstOrNull { it.conversationHex == conversation }
        ?: privateSummary?.takeIf { it.conversationHex == conversation }
        ?: chats.firstOrNull { it.kind == 0 && it.peerHex == peer }
    val hidden = chat != null && chat.conversationHex in privateIds
    val secret = chats.firstOrNull { it.isSecret && it.peerHex == peer }
    val canSecret = chat?.kind == 0 && chat.status == 1 && !chat.request
    var removingLock by remember { mutableStateOf(false) }
    var starting by remember { mutableStateOf(false) }
    val directConversation = chat?.conversationHex ?: conversation
    val picture = rememberAvatar(peer)
    val name = profile?.nickname?.ifBlank { profile?.name.orEmpty() }?.ifBlank { chat?.name.orEmpty() }
        .orEmpty().ifBlank { fallbackName }
    var nickname by rememberSaveable { mutableStateOf<String?>(null) }
    var deleting by rememberSaveable { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var debug by remember { mutableStateOf<String?>(null) }
    val mediaCount by remember(directConversation) {
        observeQuery(setOf("messages", "message_media", "private_conversations")) {
            directConversation?.let {
                runCatching {
                    val id = it.fromHex()
                    val private = CoreBridge.privateConversationIds().any { candidate -> candidate.contentEquals(id) }
                    if (private && !CoreBridge.isPrivateConversationUnlocked(id)) 0
                    else CoreBridge.sharedMedia(id).size
                }.getOrDefault(0)
            } ?: 0
        }
    }.collectAsState(0)

    val presenceMap by CoreBridge.presenceByPeer.collectAsState()
    val presence = presenceMap[peer]
    val status = presenceText(presence, rememberPresenceTime())
    val c = Promtuz.colors
    ProfileScaffold(name = name, photo = { size ->
        Avatar(name, size, image = picture, identityKey = peer, originKey = "contact:$peer",
            onClick = picture?.let { { MediaViewer.open(listOf(pictureItem("contact:$peer", it, name))) } })
    }, subtitle = status, subtitleColor = if (presence == Presence.Online) c.accent else c.muted, actions = {
        AppDropMenu(anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Contact options") }, groups = listOf(
            buildList {
                if (profile?.canShare == true) add(MenuAction("Share contact", R.drawable.oi_export) { app.navigator.push(Routes.ContactCard(peer, profile?.name.orEmpty(), true)) })
                if (chat != null && !chat.request) {
                    if (hidden) add(MenuAction("Remove lock", R.drawable.oi_lock_chat_open) { removingLock = true })
                    else add(MenuAction("Lock chat", R.drawable.oi_lock_chat) { app.navigator.push(Routes.LockChat(chat.conversationHex)) })
                }
                if (canSecret) add(MenuAction(if (secret != null) "Open secret chat" else "Start secret chat", iconPlaceholder = "secret") {
                    if (secret != null) app.openChat(secret.conversationHex, secret.name)
                    else if (!starting) {
                        starting = true
                        error = null
                        app.startSecretChat(peer, name) { starting = false; error = it }
                    }
                })
            },
            buildList {
                if (BuildConfig.DEBUG && !hidden) add(MenuAction("Debug info", R.drawable.oi_info) {
                    scope.launch {
                        debug = runCatching { CoreBridge.contactsDiag().firstOrNull { it.ipk.contentEquals(peer.fromHex()) }?.let {
                            "Identity\n$peer\n\nMLS epoch: ${it.epoch ?: "-"}\nMessages: ${it.messageCount}\nPending operations: ${it.pendingOps}"
                        } }.getOrNull() ?: "No diagnostics available"
                    }
                })
            },
            buildList { if (chat != null && !chat.isSecret) add(MenuAction("Delete contact", R.drawable.oi_trash, destructive = true) { deleting = true }) },
        ).filter { it.isNotEmpty() })
    }) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(top = 12.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)) {
            item("actions") {
                ProfileActions(buildList {
                    // Without a chat, their side receives it as a message request.
                    add(MenuAction("Message", R.drawable.i_send) {
                        if (chat != null) app.openChat(chat.conversationHex, name) else app.openChatWith(peer, name)
                    })
                    if (chat != null) add(MenuAction(if (chat.muted) "Unmute" else "Mute",
                        if (chat.muted) R.drawable.i_speaker else R.drawable.i_volume_off) {
                        scope.launch { runCatching { CoreBridge.setConversationMuted(chat.conversationHex.fromHex(), !chat.muted) }
                            .onFailure { error = "Couldn't change notifications" } }
                    })
                })
            }
            val realName = profile?.name.orEmpty().takeIf { it.isNotBlank() && it != name }
            val bio = profile?.bio.orEmpty()
            if (realName != null || bio.isNotBlank() || chat != null) item("about") {
                SettingsCard {
                    if (realName != null) SettingsRow(realName, subtitle = "Profile name")
                    if (bio.isNotBlank()) SettingsRow(bio, subtitle = "About")
                    if (chat != null) SettingsRow(profile?.nickname?.ifBlank { null } ?: "Add a nickname",
                        subtitle = "Nickname, only you see it", trailing = Trailing.Chevron,
                        onClick = { nickname = profile?.nickname.orEmpty(); error = null })
                }
            }
            if (mediaCount > 0 && directConversation != null) item("media") {
                SettingsCard {
                    SettingsRow("Shared media", icon = R.drawable.i_image, value = mediaCount.toString(), trailing = Trailing.Chevron,
                        onClick = { app.navigator.push(Routes.SharedMedia(directConversation, name)) })
                }
            }
            error?.let { item("error") { Text(it, Modifier.padding(horizontal = 16.dp), style = FootnoteStyle, color = c.danger) } }
        }
    }
    nickname?.let { draft -> AppAlertDialog(onDismissRequest = { if (!busy) nickname = null }, title = { Text("Nickname") },
        text = { Column {
            AppTextField(draft, { if (it.length <= 32) nickname = it }, singleLine = true, enabled = !busy,
                label = "Name", supportingText = { Text("Leave empty to use their profile name.") })
            error?.let { Text(it, style = FootnoteStyle, color = Promtuz.colors.danger) }
        } },
        confirmButton = { TextButton(enabled = !busy, onClick = {
            busy = true; scope.launch {
                runCatching { CoreBridge.setContactNickname(peer.fromHex(), draft) }
                    .onSuccess { nickname = null }.onFailure { error = "Couldn't save nickname" }
                busy = false
            }
        }) { Text("Save") } }, dismissButton = { TextButton(enabled = !busy, onClick = { nickname = null }) { Text("Cancel") } }) }
    if (deleting && chat != null) AppAlertDialog(onDismissRequest = { deleting = false }, title = { Text("Delete contact?") },
        text = { Text("This removes $name and your chat history from this device.") },
        confirmButton = { TextButton(onClick = { deleting = false; app.deleteChat(chat) { app.navigator.openExternal(Routes.App) } }) { Text("Delete") } },
        dismissButton = { TextButton(onClick = { deleting = false }) { Text("Cancel") } })
    debug?.let { AppAlertDialog(onDismissRequest = { debug = null }, title = { Text("Debug info") }, text = { Text(it) },
        confirmButton = { TextButton(onClick = { debug = null }) { Text("Close") } }) }
    if (removingLock && chat != null) RemoveLockDialog(app, chat.conversationHex, chat.name) { removingLock = false }
}
