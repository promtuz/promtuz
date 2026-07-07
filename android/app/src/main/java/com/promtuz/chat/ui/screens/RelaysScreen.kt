package com.promtuz.chat.ui.screens

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.RelayStatus
import com.promtuz.chat.presentation.viewmodel.RelaysVM
import com.promtuz.chat.presentation.viewmodel.UiRelay
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

@Composable
fun RelaysScreen(viewModel: RelaysVM = koinViewModel()) {
    val relays by viewModel.relays.collectAsState()

    SimpleScreen({ Text("Relay Nodes") }) { padding ->
        LazyColumn(
            Modifier
                .fillMaxSize()
                .padding(padding),
            contentPadding = PaddingValues(16.dp, 8.dp, 16.dp, 40.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp)
        ) {
            item { RelaySummary(relays) }
            items(relays, key = { it.id }) { relay ->
                RelayCard(
                    relay,
                    onConnect = { viewModel.connect(relay.id) },
                    onReset = { viewModel.resetCircuit(relay.id) },
                    onForget = { viewModel.forget(relay.id) }
                )
            }
            if (relays.isEmpty()) {
                item {
                    Text(
                        "No relays stored yet.",
                        Modifier.padding(top = 24.dp),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodyLarge
                    )
                }
            }
        }
    }
}

@Composable
private fun RelaySummary(relays: List<UiRelay>) {
    val connected = relays.count { it.isConnected }
    Text(
        "${relays.size} relays · $connected connected",
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.labelLarge
    )
}

@Composable
private fun RelayCard(
    relay: UiRelay, onConnect: () -> Unit, onReset: () -> Unit, onForget: () -> Unit
) {
    val colors = MaterialTheme.colorScheme
    val accent = statusColor(relay.status, colors)

    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(20.dp))
            .background(colors.surfaceContainerLow)
            .padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp)
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    "${relay.host}:${relay.port}",
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    color = colors.onSurface
                )
                Text(
                    relay.id.take(12),
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = colors.onSurfaceVariant
                )
            }
            StatusBadge(relay.status, accent)
            RelayMenu(relay, onConnect, onReset, onForget)
        }

        Row(verticalAlignment = Alignment.Bottom) {
            PingLabel(relay.lastLatencyMs)
            Spacer(Modifier.weight(1f))
            Text(
                successRate(relay),
                style = MaterialTheme.typography.labelMedium,
                color = colors.onSurfaceVariant
            )
        }

        LatencyGraph(
            relay.latencySamples,
            accent,
            Modifier
                .fillMaxWidth()
                .height(56.dp)
        )

        Text(
            metaLine(relay),
            style = MaterialTheme.typography.labelSmall,
            color = colors.onSurfaceVariant
        )
    }
}

@Composable
private fun StatusBadge(status: RelayStatus, accent: Color) {
    val label = when (status) {
        RelayStatus.LIVE -> "LIVE"
        RelayStatus.IDLE -> "IDLE"
        RelayStatus.PROBING -> "PROBING"
        RelayStatus.DOWN -> "DOWN"
    }
    Text(
        label,
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(accent.copy(alpha = 0.16f))
            .padding(horizontal = 8.dp, vertical = 3.dp),
        style = MaterialTheme.typography.labelSmall,
        fontWeight = FontWeight.Bold,
        color = accent
    )
}

@Composable
private fun RelayMenu(
    relay: UiRelay, onConnect: () -> Unit, onReset: () -> Unit, onForget: () -> Unit
) {
    val connect by rememberUpdatedState(onConnect)
    val reset by rememberUpdatedState(onReset)
    val forget by rememberUpdatedState(onForget)
    val groups = remember(relay.isConnected, relay.canReset) {
        listOf(
            buildList {
                add(MenuAction(if (relay.isConnected) "Reconnect" else "Connect") { connect() })
                if (relay.canReset) add(MenuAction("Reset circuit") { reset() })
            },
            listOf(MenuAction("Forget relay", destructive = true) { forget() }),
        )
    }
    AppDropMenu(
        anchor = {
            DrawableIcon(
                R.drawable.i_ellipsis_vertical,
                Modifier.padding(4.dp),
                desc = "Actions",
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        },
        groups = groups,
    )
}

@Composable
private fun PingLabel(latencyMs: Long?) {
    val text = latencyMs?.let { "$it ms" } ?: "—"
    Text(
        text,
        style = MaterialTheme.typography.headlineSmall,
        fontWeight = FontWeight.SemiBold,
        color = pingColor(latencyMs)
    )
}

/**
 * Smooth (cubic-bezier) latency line with the sample points drawn on it and a
 * soft gradient fill. x = sample index, y = latency normalized to this relay's
 * own min/max. Recomposes as the VM polls, so it animates live.
 */
@Composable
private fun LatencyGraph(samples: List<Float>, color: Color, modifier: Modifier) {
    Canvas(modifier) {
        if (samples.isEmpty()) return@Canvas

        val pad = 6.dp.toPx()
        val usableH = (size.height - pad * 2).coerceAtLeast(1f)
        val maxV = samples.max()
        val minV = samples.min()
        val range = (maxV - minV).coerceAtLeast(1f)

        // A single sample has no line to draw — mark it as a centered dot.
        if (samples.size == 1) {
            drawCircle(color, radius = 2.5.dp.toPx(), center = Offset(size.width / 2, size.height / 2))
            return@Canvas
        }

        val dx = size.width / (samples.size - 1)
        val points = samples.mapIndexed { i, v ->
            Offset(i * dx, pad + usableH * (1f - (v - minV) / range))
        }

        val line = Path().apply {
            moveTo(points[0].x, points[0].y)
            for (i in 1 until points.size) {
                val prev = points[i - 1]
                val cur = points[i]
                val midX = (prev.x + cur.x) / 2f
                cubicTo(midX, prev.y, midX, cur.y, cur.x, cur.y)
            }
        }

        val fill = Path().apply {
            addPath(line)
            lineTo(points.last().x, size.height)
            lineTo(points.first().x, size.height)
            close()
        }
        drawPath(fill, Brush.verticalGradient(listOf(color.copy(alpha = 0.22f), color.copy(alpha = 0f))))
        drawPath(
            line,
            color,
            style = Stroke(width = 2.dp.toPx(), cap = StrokeCap.Round, join = StrokeJoin.Round)
        )
        points.forEach { drawCircle(color, radius = 1.6.dp.toPx(), center = it) }
    }
}

private fun statusColor(status: RelayStatus, colors: androidx.compose.material3.ColorScheme): Color =
    when (status) {
        RelayStatus.LIVE -> Color(0xFF4CAF50)
        RelayStatus.IDLE -> colors.onSurfaceVariant
        RelayStatus.PROBING -> Color(0xFFFFA726)
        RelayStatus.DOWN -> colors.error
    }

private fun pingColor(latencyMs: Long?): Color = when {
    latencyMs == null -> Color(0xFF9E9E9E)
    latencyMs < 80 -> Color(0xFF4CAF50)
    latencyMs < 200 -> Color(0xFFFFA726)
    else -> Color(0xFFEF5350)
}

private fun successRate(relay: UiRelay): String {
    if (relay.windowAttempts == 0) return "no attempts yet"
    val pct = relay.windowSuccesses * 100 / relay.windowAttempts
    val fails = if (relay.consecutiveFailures > 0) " · ${relay.consecutiveFailures} fails" else ""
    return "$pct% · ${relay.windowSuccesses}/${relay.windowAttempts}$fails"
}

private fun metaLine(relay: UiRelay): String {
    val now = System.currentTimeMillis()
    // last_connect (set on every successful connect) is meaningful; last_seen
    // only moves when the resolver re-runs, so it'd read stale for everyone.
    val conn = relay.lastConnectMs
    val base = when {
        relay.isConnected && conn != null -> "up ${relativeDuration(now - conn)}"
        conn != null -> "last up ${relativeDuration(now - conn)} ago"
        else -> "never connected"
    }
    val backoff = relay.backoffUntilMs?.takeIf { it > now }
        ?.let { " · retry in ${relativeDuration(it - now)}" } ?: ""
    return base + backoff
}

private fun relativeDuration(ms: Long): String {
    val s = (ms / 1000).coerceAtLeast(0)
    return when {
        s < 60 -> "${s}s"
        s < 3600 -> "${s / 60}m"
        s < 86400 -> "${s / 3600}h"
        else -> "${s / 86400}d"
    }
}
