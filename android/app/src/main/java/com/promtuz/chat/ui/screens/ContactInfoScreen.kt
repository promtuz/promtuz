package com.promtuz.chat.ui.screens

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
    val chat = chats.firstOrNull { !it.isGroup && (it.conversationHex == conversation || it.peerHex == peer) }
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
        observeQuery(setOf("messages", "message_media")) { directConversation?.let { CoreBridge.sharedMedia(it.fromHex()).size } ?: 0 }
    }.collectAsState(0)

    ProfileScaffold(name = name, photo = { size ->
        Avatar(name, size, image = picture, identityKey = peer, originKey = "contact:$peer",
            onClick = picture?.let { { MediaViewer.open(listOf(pictureItem("contact:$peer", it, name))) } })
    }, actions = {
        AppDropMenu(anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Contact options") }, groups = buildList {
            if (profile?.canShare == true) add(listOf(MenuAction("Share contact", R.drawable.oi_export) { app.navigator.push(Routes.ContactCard(peer, profile?.name.orEmpty(), true)) }))
            if (BuildConfig.DEBUG) add(listOf(MenuAction("Debug info", R.drawable.oi_info) {
                scope.launch {
                    debug = runCatching { CoreBridge.contactsDiag().firstOrNull { it.ipk.contentEquals(peer.fromHex()) }?.let {
                        "Identity\n$peer\n\nMLS epoch: ${it.epoch ?: "—"}\nMessages: ${it.messageCount}\nPending operations: ${it.pendingOps}"
                    } }.getOrNull() ?: "No diagnostics available"
                }
            }))
            if (chat != null) add(listOf(MenuAction("Delete contact", R.drawable.oi_trash, destructive = true) { deleting = true }))
        })
    }) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(start = 18.dp, end = 18.dp,
            top = padding.calculateTopPadding() + 8.dp, bottom = padding.calculateBottomPadding() + 24.dp),
            horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(4.dp)) {
            item {
                if (!profile?.nickname.isNullOrBlank() && profile?.name != name)
                    Text(profile?.name.orEmpty(), color = MaterialTheme.colorScheme.onSurfaceVariant)
                if (!profile?.bio.isNullOrBlank()) Text(profile!!.bio, Modifier.padding(vertical = 12.dp), style = MaterialTheme.typography.bodyLarge)
                Spacer(Modifier.height(20.dp))
            }
            // Their side receives it as a message request.
            if (chat == null) item {
                Button(onClick = { app.openChatWith(peer, name) }) { Text("Message") }
            }
            if (chat != null) item {
                Button(onClick = { app.openChat(chat.conversationHex, name) }) { Text("Message") }
                Spacer(Modifier.height(16.dp))
                GroupedActionRow("Nickname", 0, 2, supportingText = profile?.nickname?.ifBlank { "Only visible to you" },
                    onClick = { nickname = profile?.nickname.orEmpty(); error = null }) { DrawableIcon(R.drawable.i_user, size = 26.dp) }
            }
            if (chat != null) item {
                GroupedActionRow("Notifications", 1, 2, supportingText = if (chat.muted) "Muted" else "On",
                    onClick = { scope.launch { runCatching { CoreBridge.setConversationMuted(chat.conversationHex.fromHex(), !chat.muted) }
                        .onFailure { error = "Couldn't change notifications" } } }) { DrawableIcon(R.drawable.i_notifications, size = 26.dp) }
            }
            if (mediaCount > 0 && directConversation != null) item {
                Spacer(Modifier.height(16.dp))
                GroupedActionRow("Shared media", 0, 1, supportingText = if (mediaCount == 1) "1 attachment" else "$mediaCount attachments",
                    onClick = { app.navigator.push(Routes.SharedMedia(directConversation, name)) }) { DrawableIcon(R.drawable.oi_image, size = 26.dp) }
            }
            error?.let { item { Text(it, Modifier.padding(12.dp), color = MaterialTheme.colorScheme.error) } }
        }
    }
    nickname?.let { draft -> AppAlertDialog(onDismissRequest = { if (!busy) nickname = null }, title = { Text("Nickname") },
        text = { Column {
            OutlinedTextField(draft, { if (it.length <= 32) nickname = it }, singleLine = true, enabled = !busy,
                label = { Text("Name") }, supportingText = { Text("Leave empty to use their profile name.") })
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
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
}
