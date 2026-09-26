package com.promtuz.chat.ui.screens

import android.content.Intent
import android.util.Base64
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.*
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.launch
import org.koin.compose.koinInject

@Composable
fun ContactCardScreen(route: Routes.ContactCard) {
    val context = LocalContext.current
    val app = koinInject<AppVM>()
    val scope = rememberCoroutineScope()
    var bytes by remember { mutableStateOf<ByteArray?>(null) }
    var preview by remember { mutableStateOf<uniffi.core.ContactCardPreview?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var sent by remember { mutableStateOf(false) }
    LaunchedEffect(route) {
        try {
            val card = route.encoded?.let { Base64.decode(it, Base64.URL_SAFE or Base64.NO_WRAP or Base64.NO_PADDING) }
                ?: CoreBridge.contactCard(route.peer.fromHex())
            preview = CoreBridge.previewContactCard(card)
            bytes = card
        } catch (_: Exception) { error = "This contact card isn't available" }
    }
    SimpleScreen({ Text("Contact card") }) { padding ->
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding() + 32.dp).padding(horizontal = 24.dp),
            horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(16.dp)) {
            preview?.let { card ->
                Avatar(card.name, 96.dp, identityKey = card.ipk.toHex())
                Text(card.name, style = MaterialTheme.typography.headlineSmall)
                Text(card.ipk.toHex().chunked(8).joinToString(" "), style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
                if (route.sharing) {
                    Button(onClick = {
                        val encoded = Base64.encodeToString(bytes!!, Base64.URL_SAFE or Base64.NO_WRAP or Base64.NO_PADDING)
                        val link = "https://promtuz.dev/contact#$encoded"
                        context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).setType("text/plain")
                            .putExtra(Intent.EXTRA_TEXT, "${card.name}\n$link"), "Share contact"))
                    }) { Text("Share contact") }
                } else if (card.alreadyContact) {
                    Button(onClick = { app.openChatWith(card.ipk.toHex(), card.name) }) { Text("Message") }
                } else if (sent || card.pending) {
                    Text("Request sent", color = MaterialTheme.colorScheme.primary)
                    Text("${card.name} can accept your request to start chatting.", style = MaterialTheme.typography.bodyMedium)
                } else {
                    Text("Send a contact request to ${card.name}? They'll see your name before deciding.")
                    Button(enabled = !busy, onClick = {
                        busy = true
                        scope.launch {
                            runCatching { CoreBridge.requestContact(bytes!!) }.onSuccess { sent = true }
                                .onFailure { error = "Couldn't send the request. Try again." }
                            busy = false
                        }
                    }) { Text(if (busy) "Sending…" else "Send request") }
                }
            } ?: if (error == null) CircularProgressIndicator() else Unit
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        }
    }
}

@Composable
fun ContactRequestsScreen() {
    if (com.promtuz.chat.navigation.LocalNavForeground.current) androidx.lifecycle.compose.LifecycleResumeEffect(Unit) {
        com.promtuz.core.push.PushNotifier.viewingRequests(true)
        onPauseOrDispose { com.promtuz.core.push.PushNotifier.viewingRequests(false) }
    }
    val app = koinInject<AppVM>()
    val rows by remember { observeQuery(setOf("contact_requests", "contacts")) { CoreBridge.contactRequests() } }.collectAsState(null)
    SimpleScreen({ Text("Contact requests") }) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
            if (rows == null) item { CircularProgressIndicator(Modifier.padding(24.dp)) }
            else if (rows!!.isEmpty()) item { Text("No contact requests", Modifier.padding(24.dp), color = MaterialTheme.colorScheme.onSurfaceVariant) }
            items(rows.orEmpty(), key = { "${it.outgoing}:${it.ipk.toHex()}" }) { request ->
                ListItem(modifier = Modifier.clickable { app.navigator.push(Routes.ContactRequest(request.ipk.toHex(), request.outgoing)) },
                    leadingContent = { Avatar(request.name, identityKey = request.ipk.toHex()) },
                    headlineContent = { Text(request.name) },
                    supportingContent = { Text(if (request.outgoing) "Request sent" else "Wants to connect") })
            }
        }
    }
}

@Composable
fun ContactRequestScreen(peer: String, outgoing: Boolean) {
    if (com.promtuz.chat.navigation.LocalNavForeground.current) androidx.lifecycle.compose.LifecycleResumeEffect(Unit) {
        com.promtuz.core.push.PushNotifier.viewingRequests(true)
        onPauseOrDispose { com.promtuz.core.push.PushNotifier.viewingRequests(false) }
    }
    val app = koinInject<AppVM>()
    val scope = rememberCoroutineScope()
    val rows by remember { observeQuery(setOf("contact_requests")) { CoreBridge.contactRequests() } }.collectAsState(null)
    val request = rows?.firstOrNull { it.ipk.toHex() == peer && it.outgoing == outgoing }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    SimpleScreen({ Text("Contact request") }) { padding ->
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding() + 32.dp).padding(horizontal = 24.dp),
            horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(16.dp)) {
            if (rows == null) CircularProgressIndicator()
            else if (request == null) Text("This request is no longer available")
            else {
                Avatar(request.name, 96.dp, identityKey = peer)
                Text(request.name, style = MaterialTheme.typography.headlineSmall)
                Text(peer.chunked(8).joinToString(" "), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                Text(if (outgoing) "Waiting for ${request.name} to accept." else "${request.name} wants to connect with you.")
                if (!outgoing) Button(enabled = !busy, onClick = {
                    busy = true
                    scope.launch {
                        runCatching { CoreBridge.acceptContactRequest(peer.fromHex()) }.onSuccess {
                            val conv = CoreBridge.conversationWith(peer.fromHex())
                            app.navigator.back()
                            app.navigator.openExternal(Routes.Chat(conv.toHex(), request.name))
                        }.onFailure { error = "Couldn't accept the request. Check your connection and try again." }
                        busy = false
                    }
                }) { Text(if (busy) "Connecting…" else "Accept") }
                TextButton(enabled = !busy, onClick = {
                    busy = true
                    scope.launch {
                        runCatching { CoreBridge.dismissContactRequest(peer.fromHex(), outgoing) }
                            .onSuccess { app.navigator.back() }.onFailure { error = "Couldn't dismiss request" }
                        busy = false
                    }
                }) { Text(if (outgoing) "Cancel request" else "Decline") }
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            }
        }
    }
}
