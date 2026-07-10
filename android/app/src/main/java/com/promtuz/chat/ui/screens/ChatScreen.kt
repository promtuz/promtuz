package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.DashedHorizontalDivider
import com.promtuz.chat.ui.components.MessageBubble
import com.promtuz.chat.ui.components.rememberChatWallpaper
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import kotlin.math.abs

private sealed interface ChatRow {
    data class Msg(val msg: UiMessage, val mergedTop: Boolean, val mergedBottom: Boolean) : ChatRow
    data class Frontier(val label: String) : ChatRow
}

@Composable
fun ChatScreen(name: String, viewModel: ChatVM) {
    val messages by viewModel.messages.collectAsState()
    val appearance = LocalChatAppearance.current
    val layout = appearance.layout
    val mergeWindowMs = layout.mergeWindowSecs * 1000L
    val wallpaper = rememberChatWallpaper(appearance.wallpaper)
    val hazeState = rememberHazeState()

    val rows = remember(messages, mergeWindowMs) { buildChatRows(messages, mergeWindowMs) }

    Scaffold(
        topBar = { ChatTopBar(name, viewModel, hazeState) },
        bottomBar = { ChatBottomBar(viewModel, hazeState) },
    ) { padding ->
        // Wallpaper + list are the haze source; the translucent bars sample them. contentPadding
        // (not Modifier.padding) so messages scroll *under* the bars for the blur to have something.
        Box(Modifier.fillMaxSize().then(wallpaper).hazeSource(hazeState)) {
            LazyColumn(
                Modifier.fillMaxSize(),
                contentPadding = padding,
                reverseLayout = true,
            ) {
                items(rows, key = ::rowKey) { row ->
                    when (row) {
                        is ChatRow.Msg -> {
                            val gapAbove = if (row.mergedTop) layout.messageGap.dp else layout.groupGap.dp
                            MessageBubble(Modifier.padding(top = gapAbove), row.msg, row.mergedTop, row.mergedBottom)
                        }
                        is ChatRow.Frontier -> FrontierMarker(row.label)
                    }
                }
            }
        }
    }
}

/**
 * A subtle right-aligned frontier line — "everything above is [label]". Deliberately short and
 * right-of-column so it never reads as a (centered) day separator. Delivery state shows here once
 * per tier, not per bubble (receipts are a high-water-mark).
 */
@Composable
private fun FrontierMarker(label: String) {
    val colors = MaterialTheme.colorScheme
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 3.dp),
        horizontalArrangement = Arrangement.End,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DashedHorizontalDivider(Modifier.weight(1f), color = colors.onSurfaceVariant.copy(alpha = 0.25f))
        Text(
            label.uppercase(),
            style = MaterialTheme.typography.labelSmall,
            color = colors.onSurfaceVariant.copy(alpha = 0.5f),
            modifier = Modifier.padding(start = 6.dp),
        )
    }
}

private fun rowKey(row: ChatRow): Any = when (row) {
    is ChatRow.Msg -> row.msg.key
    is ChatRow.Frontier -> "frontier:${row.label}"
}

/**
 * Interleave message rows (with group merge flags) and status-frontier markers. Each frontier is the
 * newest outgoing message of its tier (lowest index in the newest-first list); the marker sits just
 * below it, so "above the line" is that status or better. Absent tiers produce no marker.
 */
private fun buildChatRows(messages: List<UiMessage>, mergeWindowMs: Long): List<ChatRow> {
    fun frontier(status: SendStatus) = messages.indexOfFirst { it.outgoing && it.status == status }
    val seen = frontier(SendStatus.Read)
    val delivered = frontier(SendStatus.Delivered)
    val sent = frontier(SendStatus.Sent)

    val rows = ArrayList<ChatRow>(messages.size + 3)
    for (i in messages.indices) {
        when (i) {
            seen -> rows.add(ChatRow.Frontier("Seen"))
            delivered -> rows.add(ChatRow.Frontier("Delivered"))
            sent -> rows.add(ChatRow.Frontier("Sent"))
        }
        val m = messages[i]
        val older = messages.getOrNull(i + 1)
        val newer = messages.getOrNull(i - 1)
        val mergedTop = older != null && sameGroup(m, older, mergeWindowMs)
        val mergedBottom = newer != null && sameGroup(m, newer, mergeWindowMs)
        rows.add(ChatRow.Msg(m, mergedTop, mergedBottom))
    }
    return rows
}

private fun sameGroup(a: UiMessage, b: UiMessage, windowMs: Long): Boolean =
    a.outgoing == b.outgoing && abs(a.timestampMs - b.timestampMs) <= windowMs
