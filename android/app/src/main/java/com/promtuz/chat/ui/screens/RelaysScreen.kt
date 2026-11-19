package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.RelayStatus
import com.promtuz.chat.presentation.viewmodel.RelaysVM
import com.promtuz.chat.presentation.viewmodel.UiRelay
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import org.koin.androidx.compose.koinViewModel

@Composable
fun RelaysScreen(viewModel: RelaysVM = koinViewModel()) {
    val relays by viewModel.relays.collectAsState()

    GroupedScreen({ Text("Relays") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            StatusCard(relays)
            if (relays.isNotEmpty()) {
                CardGap()
                SettingsCard(header = "Your relays", footer = "Tap a relay to reconnect or forget it.") {
                    relays.forEach { relay ->
                        // Two relays on one host are told apart by port; otherwise the port is noise.
                        val shared = relays.count { it.host == relay.host } > 1
                        RelayRow(
                            relay, shared,
                            onConnect = { viewModel.connect(relay.id) },
                            onReset = { viewModel.resetCircuit(relay.id) },
                            onForget = { viewModel.forget(relay.id) },
                        )
                    }
                }
            }
        }
    }
}

/** How reachable the network is, in words; any one relay is enough. */
@Composable
private fun StatusCard(relays: List<UiRelay>) {
    val c = Promtuz.colors
    val up = relays.count { it.isConnected }
    val (title, body, dot) = when {
        relays.isEmpty() -> Triple("No relays yet", "Promtuz finds relays when it connects to the network.", c.muted)
        up == 0 -> Triple("Not connected", "No relay is reachable right now. Messages wait on this phone until one is.", c.danger)
        up == relays.size && up == 1 -> Triple("Connected", "Your relay is reachable.", c.success)
        up == relays.size -> Triple("Connected", "All $up relays are reachable.", c.success)
        else -> Triple("Connected", "$up of ${relays.size} relays are reachable. One is enough to send and receive messages.", c.success)
    }
    Column(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(cardFill()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Box(Modifier.size(10.dp).clip(RoundedCornerShape(3.dp)).background(dot))
            Text(title, style = TextStyle(fontSize = 20.sp, lineHeight = 26.sp, fontWeight = FontWeight.Medium), color = c.text)
        }
        Text(body, style = RowSubtitleStyle, color = c.muted)
    }
}

@Composable
private fun RelayRow(relay: UiRelay, showPort: Boolean, onConnect: () -> Unit, onReset: () -> Unit, onForget: () -> Unit) {
    val c = Promtuz.colors
    val connect by rememberUpdatedState(onConnect)
    val reset by rememberUpdatedState(onReset)
    val forget by rememberUpdatedState(onForget)
    val groups = remember(relay.isConnected, relay.canReset) {
        listOf(
            buildList {
                add(MenuAction(if (relay.isConnected) "Reconnect" else "Connect") { connect() })
                if (relay.canReset) add(MenuAction("Try again now") { reset() })
            },
            listOf(MenuAction("Forget relay", destructive = true) { forget() }),
        )
    }
    val tile = 40.dp
    AppDropMenu(
        modifier = Modifier.cardRow(16.dp + tile + 14.dp),
        anchor = {
            Row(
                Modifier.fillMaxWidth().heightIn(min = 64.dp).padding(horizontal = 16.dp, vertical = 10.dp),
                horizontalArrangement = Arrangement.spacedBy(14.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(Modifier.size(tile)) {
                    Box(
                        Modifier.matchParentSize().clip(RoundedCornerShape(Radii.small)).background(c.cardHigh),
                        contentAlignment = Alignment.Center,
                    ) { DrawableIcon(R.drawable.i_network_nodes, size = 22.dp, tint = c.muted) }
                    // The status mark sits on the tile's corner, ringed in the card colour.
                    Box(
                        Modifier.align(Alignment.BottomEnd).offset(3.dp, 3.dp).size(14.dp)
                            .clip(RoundedCornerShape(5.dp)).background(cardFill()).padding(2.dp)
                            .clip(RoundedCornerShape(3.dp)).background(statusColor(relay.status)),
                    )
                }
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text(displayHost(relay, showPort), style = RowTitleStyle, color = c.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Text(
                        statusLine(relay), style = RowSubtitleStyle,
                        color = if (relay.status == RelayStatus.DOWN) c.danger else c.muted,
                    )
                }
            }
        },
        groups = groups,
        onLongClickLabel = "Relay options",
    )
}

private fun displayHost(relay: UiRelay, showPort: Boolean): String {
    val host = relay.host.removePrefix("::ffff:")
    return if (showPort) "$host:${relay.port}" else host
}

private fun statusLine(relay: UiRelay): String {
    val now = System.currentTimeMillis()
    return when (relay.status) {
        RelayStatus.LIVE -> relay.lastLatencyMs?.let { "Connected · $it ms" } ?: "Connected"
        RelayStatus.PROBING -> "Checking…"
        RelayStatus.IDLE -> if (relay.consecutiveFailures > 0) "Unreachable" else "Available"
        RelayStatus.DOWN -> relay.backoffUntilMs?.takeIf { it > now }
            ?.let { "Unreachable · retrying in ${relativeDuration(it - now)}" } ?: "Unreachable"
    }
}

@Composable
private fun statusColor(status: RelayStatus): Color {
    val c = Promtuz.colors
    return when (status) {
        RelayStatus.LIVE -> c.success
        RelayStatus.IDLE -> c.muted
        RelayStatus.PROBING -> c.warn
        RelayStatus.DOWN -> c.danger
    }
}

private fun relativeDuration(ms: Long): String {
    val s = (ms / 1000).coerceAtLeast(1)
    return when {
        s < 60 -> "$s s"
        s < 3600 -> "${s / 60} min"
        s < 86400 -> "${s / 3600} h"
        else -> "${s / 86400} d"
    }
}
