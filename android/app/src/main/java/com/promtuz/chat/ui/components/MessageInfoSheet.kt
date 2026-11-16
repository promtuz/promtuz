package com.promtuz.chat.ui.components

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.updateTransition
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.domain.model.previewLine
import com.promtuz.chat.R
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.animation.QueuedAnimatedNumber
import com.promtuz.chat.ui.text.dateAndTime
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.chat.utils.media.rememberMessagePreview
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.CancellationException
import uniffi.core.MessageReceiptInfo
import uniffi.core.RecipientReceipt

@Composable
fun MessageInfoSheet(conversationHex: String, message: UiMessage, onDismiss: () -> Unit) {
    val dispatches = remember(message.key) {
        (message.content as? MessageContent.Album)?.items?.map { it.dispatchIdHex.fromHex() }
            ?: listOf(message.dispatchIdHex.fromHex())
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
        LazyColumn(Modifier.fillMaxWidth().heightIn(max = maxHeight),
            contentPadding = PaddingValues(bottom = bottom + 16.dp)) {
            item("message") {
                Text("Message info", Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.titleLarge)
                MessageInfoPreview(conversationHex, message)
            }
            if (error) item("error") {
                Row(Modifier.fillMaxWidth().padding(horizontal = 24.dp), verticalAlignment = Alignment.CenterVertically) {
                    Text("Couldn’t load message info", Modifier.weight(1f), color = colors.error)
                    TextButton(onClick = { attempt++ }) { Text("Retry") }
                }
            } else if (info == null) item("loading") {
                Box(Modifier.fillMaxWidth().padding(24.dp), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
                }
            }
            info?.let { details ->
                if (!details.complete) item("incomplete") {
                    Text("Some details weren’t recorded for this older message.", Modifier.padding(horizontal = 24.dp, vertical = 12.dp),
                        style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant)
                }
                if (details.complete && details.recipients.isEmpty()) item("empty") {
                    Text("No recipients", Modifier.padding(horizontal = 24.dp, vertical = 12.dp), color = colors.onSurfaceVariant)
                }
                val groups = details.recipients.groupBy { SendStatus.from(it.status.toInt()) }
                for (status in listOf(SendStatus.Read, SendStatus.Delivered, SendStatus.Sent, SendStatus.Pending, SendStatus.Failed)) {
                    val recipients = groups[status].orEmpty()
                    if (recipients.isEmpty()) continue
                    item("section-$status") { ReceiptSection(status, recipients.size) }
                    items(recipients, key = { it.member.toHex() }) { recipient ->
                        // The accordion already moves these bounds on every frame.
                        ReceiptPerson(recipient, Modifier.animateItem(placementSpec = null))
                    }
                }
            }
        }
    }
}

@Composable
private fun MessageInfoPreview(conversationHex: String, message: UiMessage) {
    val colors = MaterialTheme.colorScheme
    val content = message.content
    val thumbnail = when (content) {
        is MessageContent.Image -> content.bitmap
        is MessageContent.Attachment -> content.thumb
        is MessageContent.Album -> (content.items.firstOrNull()?.content as? MessageContent.Image)?.bitmap
        is MessageContent.Sticker -> rememberMessagePreview(conversationHex, message.dispatchIdHex, retry = true)
        else -> null
    }
    val icon = when (content) {
        is MessageContent.Image, is MessageContent.Album -> R.drawable.oi_image
        is MessageContent.Attachment -> R.drawable.oi_file_attachment
        is MessageContent.Sticker -> R.drawable.oi_sticker
        is MessageContent.Voice -> R.drawable.i_mic
        else -> null
    }
    Row(Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 16.dp),
        verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(16.dp)) {
        if (thumbnail != null || icon != null) Box(
            Modifier.size(52.dp).clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.badge)).background(com.promtuz.chat.ui.theme.Promtuz.colors.cardHigh),
            contentAlignment = Alignment.Center,
        ) {
            if (thumbnail != null) Image(thumbnail, null, Modifier.fillMaxSize(),
                contentScale = if (content is MessageContent.Sticker) ContentScale.Fit else ContentScale.Crop)
            else icon?.let { DrawableIcon(it, size = 24.dp, tint = colors.onSurfaceVariant) }
        }
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(content.previewLine().ifBlank { "Message" }, style = MaterialTheme.typography.bodyLarge,
                maxLines = 2, overflow = TextOverflow.Ellipsis)
            val sent = when (message.status) {
                SendStatus.Pending -> "Sending"
                SendStatus.Failed -> "Not sent"
                else -> "Sent ${dateAndTime(LocalContext.current, message.timestampMs)}"
            }
            Text(sent,
                style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
        }
    }
}

private fun receiptLabel(status: SendStatus): String = when (status) {
    SendStatus.Pending -> "Sending"
    SendStatus.Sent -> "Sent"
    SendStatus.Failed -> "Failed"
    SendStatus.Delivered -> "Delivered"
    SendStatus.Read -> "Read"
}

@Composable
private fun ReceiptSection(status: SendStatus, count: Int) {
    val colors = MaterialTheme.colorScheme
    Column(Modifier.padding(horizontal = 24.dp)) {
        HorizontalDivider(Modifier.padding(top = 8.dp, bottom = 20.dp), color = colors.outlineVariant)
        Row(Modifier.fillMaxWidth().padding(bottom = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Text("${receiptLabel(status)} · ", style = MaterialTheme.typography.titleSmall)
            QueuedAnimatedNumber(count, style = MaterialTheme.typography.titleSmall, label = "receipt count")
            Spacer(Modifier.weight(1f))
            MessageStatusIcon(status, Modifier.size(20.dp), tint = colors.onSurfaceVariant,
                seenTint = colors.primary, errorTint = colors.error)
        }
    }
}

@Composable
private fun ReceiptPerson(receipt: RecipientReceipt, modifier: Modifier = Modifier) {
    val colors = MaterialTheme.colorScheme
    val peer = receipt.member.toHex()
    val status = SendStatus.from(receipt.status.toInt())
    var expanded by remember(peer) { mutableStateOf(false) }
    val latestTime = when (status) {
        SendStatus.Read -> receipt.readAt
        SendStatus.Delivered -> receipt.deliveredAt
        SendStatus.Sent -> receipt.sentAt
        else -> null
    }
    val hasTimeline = (receipt.sentAt != null && status != SendStatus.Sent) ||
        (receipt.deliveredAt != null && status != SendStatus.Delivered) ||
        (receipt.readAt != null && status != SendStatus.Read)
    val expansion = updateTransition(expanded && hasTimeline, label = "receipt accordion")
    val chevronRotation by expansion.animateFloat(
        transitionSpec = { Tweens.microInteraction(220) }, label = "receipt chevron",
    ) { if (it) 180f else 0f }
    Column(modifier.fillMaxWidth().clickable(enabled = hasTimeline,
        onClickLabel = if (expanded) "Hide receipt times" else "Show receipt times",
        onClick = { expanded = !expanded })) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(14.dp)) {
            Avatar(receipt.name, size = 44.dp, identityKey = peer, image = rememberAvatar(peer))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(receipt.name, style = MaterialTheme.typography.bodyLarge,
                    maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (status == SendStatus.Read || status == SendStatus.Delivered || status == SendStatus.Sent)
                    ReceiptTime(receiptLabel(status), latestTime)
                else if (status == SendStatus.Failed) Text("Couldn’t send", style = MaterialTheme.typography.bodySmall, color = colors.error)
                if (!receipt.active) Text("Left the chat", style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
            }
            if (hasTimeline) MorphIcon(
                MorphGlyph.ChevronDown,
                description = null,
                modifier = Modifier.size(18.dp).graphicsLayer { rotationZ = chevronRotation },
                tint = colors.onSurfaceVariant,
            )
        }
        expansion.AnimatedVisibility(visible = { it },
            enter = expandVertically(Tweens.microInteraction(220), expandFrom = Alignment.Top),
            exit = shrinkVertically(Tweens.microInteraction(220), shrinkTowards = Alignment.Top)) {
            Column(Modifier.padding(start = 82.dp, end = 24.dp, bottom = 12.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp)) {
                if (receipt.sentAt != null && status != SendStatus.Sent) ReceiptTime("Sent", receipt.sentAt)
                if (receipt.deliveredAt != null && status != SendStatus.Delivered) ReceiptTime("Delivered", receipt.deliveredAt)
                if (receipt.readAt != null && status != SendStatus.Read) ReceiptTime("Read", receipt.readAt)
            }
        }
    }
}

@Composable
private fun ReceiptTime(label: String, seconds: ULong?) {
    val text = if (seconds == null || seconds > (Long.MAX_VALUE / 1000).toULong()) "Time unavailable"
        else dateAndTime(LocalContext.current, seconds.toLong() * 1000)
    Text("$label $text", style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant)
}
