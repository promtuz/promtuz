package com.promtuz.chat.ui.components

import android.text.format.DateFormat
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.CancellationException
import uniffi.core.MessageReceiptInfo
import uniffi.core.RecipientReceipt
import java.util.Date

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MessageInfoSheet(conversationHex: String, message: UiMessage, onDismiss: () -> Unit) {
    val dispatches = remember(message.key) {
        (message.content as? MessageContent.Album)?.items?.map { it.dispatchIdHex.fromHex() }
            ?: listOfNotNull(message.dispatchIdHex?.fromHex())
    }
    var info by remember(message.key) { mutableStateOf<MessageReceiptInfo?>(null) }
    var error by remember(message.key) { mutableStateOf(false) }
    var attempt by remember { mutableIntStateOf(0) }
    LaunchedEffect(conversationHex, message.key, attempt) {
        error = false
        try {
            observeQuery(setOf("messages", "message_recipients", "receipt_peers", "conversation_members", "contacts", "peer_names", "peer_profiles", "prefs")) {
                CoreBridge.messageReceiptInfo(conversationHex.fromHex(), dispatches)
            }.collect { info = it }
        } catch (cancel: CancellationException) {
            throw cancel
        } catch (_: Exception) {
            error = true
        }
    }
    val colors = MaterialTheme.colorScheme
    val maxHeight = LocalConfiguration.current.screenHeightDp.dp * 0.82f
    val bottom = WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding()
    AppBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        contentWindowInsets = { WindowInsets(0) },
    ) {
        Text("Message info", Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
            style = MaterialTheme.typography.titleLarge)
        Text(messagePreview(message.content), Modifier.padding(start = 24.dp, end = 24.dp, bottom = 16.dp),
            style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant,
            maxLines = 2, overflow = TextOverflow.Ellipsis)
        LazyColumn(Modifier.fillMaxWidth().heightIn(max = maxHeight),
            contentPadding = PaddingValues(bottom = bottom + 16.dp)) {
            if (error) item {
                Row(Modifier.fillMaxWidth().padding(horizontal = 24.dp), verticalAlignment = Alignment.CenterVertically) {
                    Text("Couldn’t load message info", Modifier.weight(1f), color = colors.error)
                    TextButton(onClick = { attempt++ }) { Text("Retry") }
                }
            } else if (info == null) item {
                Box(Modifier.fillMaxWidth().padding(24.dp), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
                }
            }
            info?.let { details ->
                if (!details.complete) item {
                    Text("Some details weren’t recorded for this older message.", Modifier.padding(horizontal = 24.dp, vertical = 12.dp),
                        style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant)
                }
                if (details.complete && details.recipients.isEmpty()) item {
                    Text("No recipients", Modifier.padding(horizontal = 24.dp, vertical = 12.dp), color = colors.onSurfaceVariant)
                }
                items(details.recipients, key = { it.member.toHex() }) { recipient ->
                    ReceiptPerson(recipient)
                }
            }
        }
    }
}

private fun messagePreview(content: MessageContent): String = when (content) {
    is MessageContent.Text -> content.text
    is MessageContent.Image -> content.caption.ifBlank { "Photo" }
    is MessageContent.Album -> content.caption.ifBlank { "${content.items.size} photos" }
    is MessageContent.Attachment -> content.name
    is MessageContent.Voice -> "Voice message"
    is MessageContent.Sticker -> "Sticker"
    else -> "Message"
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ReceiptPerson(receipt: RecipientReceipt) {
    val colors = MaterialTheme.colorScheme
    val peer = receipt.member.toHex()
    val status = SendStatus.from(receipt.status.toInt())
    val label = when (status) {
        SendStatus.Pending -> "Sending"
        SendStatus.Sent -> "Sent"
        SendStatus.Failed -> "Failed"
        SendStatus.Delivered -> "Delivered"
        SendStatus.Read -> "Read"
    }
    Row(Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(16.dp)) {
        Avatar(receipt.name, size = 44.dp, identityKey = peer, image = rememberAvatar(peer))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(receipt.name, Modifier.weight(1f), style = MaterialTheme.typography.titleSmall,
                    maxLines = 2, overflow = TextOverflow.Ellipsis)
                Text(label, style = MaterialTheme.typography.labelMedium,
                    color = if (status == SendStatus.Failed) colors.error else colors.onSurfaceVariant)
            }
            if (!receipt.active) Text("Left the chat", style = MaterialTheme.typography.labelSmall, color = colors.onSurfaceVariant)
            if (receipt.sentAt != null) ReceiptTime("Sent", receipt.sentAt)
            if (status == SendStatus.Delivered || status == SendStatus.Read) ReceiptTime("Delivered", receipt.deliveredAt)
            if (status == SendStatus.Read) ReceiptTime("Read", receipt.readAt)
        }
    }
}

@Composable
private fun ReceiptTime(label: String, seconds: ULong?) {
    val context = LocalContext.current
    val text = if (seconds == null || seconds > (Long.MAX_VALUE / 1000).toULong()) "Time unavailable" else {
        val date = Date(seconds.toLong() * 1000)
        "${DateFormat.getMediumDateFormat(context).format(date)}, ${DateFormat.getTimeFormat(context).format(date)}"
    }
    Text("$label · $text", style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant)
}
