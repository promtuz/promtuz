package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CircularWavyProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.VerticalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.ConnectionStats
import com.promtuz.chat.domain.model.EndpointStats
import com.promtuz.chat.domain.model.RelayInfo
import com.promtuz.chat.domain.model.UdpStats
import com.promtuz.chat.presentation.viewmodel.NetworkStatsVM
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.core.API
import org.koin.compose.koinInject
import org.koin.compose.viewmodel.koinViewModel
import kotlin.math.roundToInt

@Composable
fun NetworkStatsScreen(
    viewModel: NetworkStatsVM = koinViewModel(),
    api: API = koinInject()
) {
    val stats by viewModel.stats.collectAsState()

    SimpleScreen({ Text("Network Stats") }, actions = {}) { padding ->
        LazyColumn(
            modifier = Modifier
                .padding(padding)
                .fillMaxSize(),
            contentPadding = PaddingValues(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            stats?.let { networkStats ->
                // val state = ConnectionState.fromInt(networkStats.state)

                networkStats.connected_relay?.let {
                    item {
                        ConnectedRelayCard(it)
                    }
                }

                // Connection Details (only if connected)
                networkStats.connection?.let { conn ->
                    item {
                        ConnectionDetailsCard(connection = conn)
                    }

                    item {
                        NetworkPerformanceCard(connection = conn)
                    }

                    item {
                        DataTransferCard(connection = conn)
                    }
                }

                // Endpoint Stats Card
                item {
                    EndpointStatsCard(endpoint = networkStats.endpoint)
                }

            } ?: item {
                // Loading state
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(32.dp),
                    contentAlignment = Alignment.Center
                ) {
                    CircularWavyProgressIndicator()
                }
            }
        }
    }
}


@Composable
fun ConnectedRelayCard(relay: RelayInfo) {
    Card(
        modifier = Modifier.fillMaxWidth()
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp)
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text(
                    text = "Connected Relay",
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.Bold
                )

                ReputationBadge(reputation = relay.reputation)
            }

            Spacer(modifier = Modifier.height(12.dp))

            InfoRow(label = "ID", value = relay.id)
            InfoRow(label = "Address", value = "${relay.host}:${relay.port}")
            relay.avg_latency?.let {
                InfoRow(label = "Avg Latency", value = "${it}ms")
            }
        }
    }
}

@Composable
fun ReputationBadge(reputation: Int) {
    val color = when {
        reputation >= 100 -> Color(0xFF00C853) // Green
        reputation >= 50 -> Color(0xFF2196F3)  // Blue
        reputation >= 10 -> Color(0xFFFFC107)  // Amber
        reputation >= 1 -> Color(0xFF9E9E9E)   // Gray
        else -> Color(0xFFF44336)              // Red
    }

    Surface(
        shape = RoundedCornerShape(4.dp),
        color = color.copy(alpha = 0.12f),
        contentColor = color
    ) {
        Text(
            text = reputation.toString(),
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.Bold,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp)
        )
    }
}

@Composable
fun EndpointStatsCard(endpoint: EndpointStats) {
    Card(
        modifier = Modifier.fillMaxWidth()
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp)
        ) {
            Text(
                text = "Endpoint Statistics",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold
            )

            Spacer(modifier = Modifier.height(12.dp))

            InfoRow(label = "Bind Address", value = endpoint.bind_addr)
            InfoRow(label = "Open Connections", value = endpoint.open_connections.toString())
            InfoRow(label = "Accepted Handshakes", value = endpoint.accepted_handshakes.toString())
            InfoRow(label = "Outgoing Handshakes", value = endpoint.outgoing_handshakes.toString())
            InfoRow(label = "Refused Handshakes", value = endpoint.refused_handshakes.toString())
            InfoRow(label = "Ignored Handshakes", value = endpoint.ignored_handshakes.toString())
        }
    }
}


@Composable
fun ConnectionDetailsCard(connection: ConnectionStats) {
    Card(
        modifier = Modifier.fillMaxWidth()
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp)
        ) {
            Text(
                text = "Connection Details",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold
            )

            Spacer(modifier = Modifier.height(12.dp))

            InfoRow(label = "Remote Address", value = connection.remote_address)
            InfoRow(label = "Uptime", value = formatDuration(connection.uptime))
            InfoRow(
                label = "RTT",
                value = "${(connection.path.rtt / 1000.0).roundToInt()}ms",
//                icon = Icons.Default.Speed
            )
            InfoRow(
                label = "Congestion Window",
                value = formatBytes(connection.path.cwnd),
//                icon = Icons.Default.Storage
            )
        }
    }
}

@Composable
fun NetworkPerformanceCard(connection: ConnectionStats) {
    Card(
        modifier = Modifier.fillMaxWidth()
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp)
        ) {
            Text(
                text = "Network Performance",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold
            )

            Spacer(modifier = Modifier.height(12.dp))

            InfoRow(
                label = "Congestion Events",
                value = connection.path.congestion_events.toString()
            )
            InfoRow(
                label = "Black Holes Detected",
                value = connection.path.black_holes_detected.toString()
            )

            Spacer(modifier = Modifier.height(8.dp))
            HorizontalDivider()
            Spacer(modifier = Modifier.height(8.dp))

            Text(
                text = "Frame Statistics",
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Spacer(modifier = Modifier.height(8.dp))

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = "Received",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    Spacer(modifier = Modifier.height(4.dp))
                    InfoRow(
                        label = "ACKs",
                        value = connection.frame_rx.acks.toString(),
                        compact = true
                    )
                    InfoRow(
                        label = "Stream",
                        value = connection.frame_rx.max_stream_data.toString(),
                        compact = true
                    )
                    InfoRow(
                        label = "Crypto",
                        value = connection.frame_rx.crypto.toString(),
                        compact = true
                    )
                    InfoRow(
                        label = "Resets",
                        value = connection.frame_rx.reset_stream.toString(),
                        compact = true
                    )
                }

                VerticalDivider(
                    modifier = Modifier
                        .height(100.dp)
                        .padding(horizontal = 12.dp)
                )

                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = "Transmitted",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    Spacer(modifier = Modifier.height(4.dp))
                    InfoRow(
                        label = "ACKs",
                        value = connection.frame_tx.acks.toString(),
                        compact = true
                    )
                    InfoRow(
                        label = "Stream",
                        value = connection.frame_tx.max_stream_data.toString(),
                        compact = true
                    )
                    InfoRow(
                        label = "Crypto",
                        value = connection.frame_tx.crypto.toString(),
                        compact = true
                    )
                    InfoRow(
                        label = "Resets",
                        value = connection.frame_tx.reset_stream.toString(),
                        compact = true
                    )
                }
            }
        }
    }
}

@Composable
fun DataTransferCard(connection: ConnectionStats) {
    Card(
        modifier = Modifier.fillMaxWidth()
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(16.dp)
        ) {
            Text(
                text = "Data Transfer",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold
            )

            Spacer(modifier = Modifier.height(12.dp))

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween
            ) {
                DataTransferColumn(
                    label = "Received",
                    stats = connection.udp_rx,
                    // icon = Icons.Default.ArrowDownward,
                    modifier = Modifier.weight(1f)
                )

                VerticalDivider(
                    modifier = Modifier
                        .height(120.dp)
                        .padding(horizontal = 12.dp)
                )

                DataTransferColumn(
                    label = "Transmitted",
                    stats = connection.udp_tx,
                    // TODO
//                    icon = Icons.Default.ArrowUpward,
                    modifier = Modifier.weight(1f)
                )
            }
        }
    }
}

@Composable
fun DataTransferColumn(
    label: String,
    stats: UdpStats,
    // icon: ImageVector,
    modifier: Modifier = Modifier
) {
    Column(
        modifier = modifier,
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
//        Icon(
//            imageVector = icon,
//            contentDescription = null,
//            tint = MaterialTheme.colorScheme.primary,
//            modifier = Modifier.size(24.dp)
//        )
//        Spacer(modifier = Modifier.height(4.dp))
        Text(
            text = label,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )
        Spacer(modifier = Modifier.height(8.dp))

        Text(
            text = formatBytes(stats.bytes),
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold
        )
        Spacer(modifier = Modifier.height(8.dp))

        InfoRow(label = "Datagrams", value = stats.datagrams.toString(), compact = true)
        InfoRow(label = "I/Os", value = stats.ios.toString(), compact = true)
    }
}

/////////////////////////////////////////////////////
///////////////// HELPER COMPONENTS /////////////////
/////////////////////////////////////////////////////

@Composable
fun InfoRow(
    label: String,
    value: String,
    // icon: ImageVector? = null,
    compact: Boolean = false
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = if (compact) 2.dp else 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp)
        ) {
//            icon?.let {
//                Icon(
//                    imageVector = it,
//                    contentDescription = null,
//                    modifier = Modifier.size(16.dp),
//                    tint = MaterialTheme.colorScheme.onSurfaceVariant
//                )
//            }
            Text(
                text = label,
                style = if (compact) MaterialTheme.typography.bodySmall
                else MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
        }
        Text(
            text = value,
            style = if (compact) MaterialTheme.typography.bodySmall
            else MaterialTheme.typography.bodyMedium,
            fontWeight = FontWeight.Medium
        )
    }
}
//
//@Composable
//fun ReputationBadge(reputation: Int) {
//    val (color, label) = when {
//        reputation >= 80 -> MaterialTheme.colorScheme.primary to "Excellent"
//        reputation >= 60 -> MaterialTheme.colorScheme.tertiary to "Good"
//        reputation >= 40 -> MaterialTheme.colorScheme.secondary to "Fair"
//        else -> MaterialTheme.colorScheme.error to "Poor"
//    }
//
//    Surface(
//        shape = MaterialTheme.shapes.small,
//        color = color,
//        contentColor = MaterialTheme.colorScheme.onPrimary
//    ) {
//        Row(
//            modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
//            verticalAlignment = Alignment.CenterVertically,
//            horizontalArrangement = Arrangement.spacedBy(4.dp)
//        ) {
//            Icon(
//                imageVector = Icons.Default.Star,
//                contentDescription = null,
//                modifier = Modifier.size(16.dp)
//            )
//            Text(
//                text = "$reputation • $label",
//                style = MaterialTheme.typography.labelMedium,
//                fontWeight = FontWeight.Bold
//            )
//        }
//    }
//}


// ============================================================================
// UTILITY FUNCTIONS
// ============================================================================

fun formatBytes(bytes: Long): String {
    return when {
        bytes < 1024 -> "$bytes B"
        bytes < 1024 * 1024 -> "${(bytes / 1024.0).roundToInt()} KB"
        bytes < 1024 * 1024 * 1024 -> "${(bytes / (1024.0 * 1024)).roundToInt()} MB"
        else -> "${(bytes / (1024.0 * 1024 * 1024)).roundToInt()} GB"
    }
}

fun formatDuration(seconds: Long): String {
    return when {
        seconds < 60 -> "${seconds}s"
        seconds < 3600 -> "${seconds / 60}m ${seconds % 60}s"
        seconds < 86400 -> "${seconds / 3600}h ${(seconds % 3600) / 60}m"
        else -> "${seconds / 86400}d ${(seconds % 86400) / 3600}h"
    }
}