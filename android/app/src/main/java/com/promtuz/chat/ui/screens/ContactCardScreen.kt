package com.promtuz.chat.ui.screens

import android.content.Intent
import android.util.Base64
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.*
import com.promtuz.core.CoreBridge
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
                } else {
                    // Someone who hasn't added us gets our first message as a request.
                    Button(enabled = !busy, onClick = {
                        busy = true
                        scope.launch {
                            runCatching { CoreBridge.chatFromCard(bytes!!) }
                                .onSuccess { app.openChat(it.toHex(), card.name) }
                                .onFailure { error = "Couldn't open a chat with ${card.name}" }
                            busy = false
                        }
                    }) { Text("Message") }
                }
            } ?: if (error == null) CircularProgressIndicator() else Unit
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        }
    }
}
