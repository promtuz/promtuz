package com.promtuz.chat.domain.model

import kotlinx.serialization.Serializable

@Serializable
data class EndpointStats(
    val open_connections: Int,
    val accepted_handshakes: Long,
    val outgoing_handshakes: Long,
    val refused_handshakes: Long,
    val ignored_handshakes: Long,
    val bind_addr: String,
)

@Serializable
data class PathStats(
    val rtt: Long,              // microseconds
    val cwnd: Long,             // congestion window
    val congestion_events: Long = 0,
    val black_holes_detected: Long = 0
)

@Serializable
data class FrameStats(
    val acks: Long, val max_stream_data: Long, val crypto: Long, val reset_stream: Long
)

@Serializable
data class UdpStats(
    val datagrams: Long,
    val bytes: Long,
    val ios: Long,
)

@Serializable
data class ConnectionStats(
    val uptime: Long,  // seconds
    val path: PathStats,
    val frame_rx: FrameStats,
    val frame_tx: FrameStats,
    val udp_rx: UdpStats,
    val udp_tx: UdpStats,
    val remote_address: String
)

@Serializable
data class RelayInfo(
    val id: String,
    val host: String,
    val port: Int,
    val reputation: Int,
    val avg_latency: Long? // ms
)

@Serializable
data class NetworkStats(
    val state: Int, // ConnectionState as i32
    val endpoint: EndpointStats,
    val connection: ConnectionStats?,
    val connected_relay: RelayInfo?,
    val version: UShort
)
