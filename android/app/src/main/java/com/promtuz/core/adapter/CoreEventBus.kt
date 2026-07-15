package com.promtuz.core.adapter

import com.promtuz.chat.presentation.state.ConnectionState
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.utils.extensions.toHex
import uniffi.core.CoreEvents
import uniffi.core.MessageEvent
import uniffi.core.ConnectionState as FfiConnectionState
import uniffi.core.Presence as FfiPresence

/** Ephemeral peer activity — `bits` is an OR of ACTIVITY_* (0 = present-idle). */
class ActivitySignal(val peer: ByteArray, val bits: Int)

/** A newly-delivered incoming message — drives push notifications (a transient projection). */
class IncomingMessage(val peerHex: String, val content: String, val timestampMs: Long)

/** Ephemeral presence delta for a contact. */
class PresenceSignal(val peer: ByteArray, val presence: Presence)

/**
 * The client's [CoreEvents] port. core calls these off-main (tokio threads);
 * bodies MUST NOT throw or block — a throw across the FFI callback aborts core,
 * and on_db_changed fires while the writing DB connection is locked. So every
 * path is a non-blocking tryEmit into a bounded, drop-oldest flow.
 *
 * Persistent state is *observed*, not pushed: [dbChanged] is the doorbell — fed
 * by the SQLite commit hook AND by the message/reaction events (belt-and-suspenders;
 * each maps to a DB write) — and the truth comes from re-reading the DB. Only the
 * genuinely ephemeral signals, [activity] and [presence], flow as typed events.
 */
object CoreEventBus : CoreEvents {
    private val _connection = MutableStateFlow(ConnectionState.Idle)
    val connection: StateFlow<ConnectionState> = _connection.asStateFlow()

    private val _dbChanged = bounded<Set<String>>()
    val dbChanged: SharedFlow<Set<String>> = _dbChanged.asSharedFlow()

    private val _activity = bounded<ActivitySignal>()
    val activity: SharedFlow<ActivitySignal> = _activity.asSharedFlow()

    private val _presence = bounded<PresenceSignal>()
    val presence: SharedFlow<PresenceSignal> = _presence.asSharedFlow()

    /** Newly-delivered incoming messages, typed — the push-notification source. */
    private val _incoming = bounded<IncomingMessage>()
    val incoming: SharedFlow<IncomingMessage> = _incoming.asSharedFlow()

    /**
     * Last-known presence per peer (hex IPK). The event stream has no memory, so a
     * delta arriving while no screen collects was simply lost until the next
     * subscribe — this cache is what late collectors read first.
     */
    private val _presenceByPeer = MutableStateFlow<Map<String, Presence>>(emptyMap())
    val presenceByPeer: StateFlow<Map<String, Presence>> = _presenceByPeer.asStateFlow()

    /** Seed the presence cache from disk on cold start ([PresenceStore]). */
    fun hydratePresence(seed: Map<String, Presence>) {
        if (seed.isNotEmpty()) _presenceByPeer.value = seed
    }

    override fun onConnection(state: FfiConnectionState) {
        _connection.value = ConnectionState.entries.getOrElse(state.ordinal) { ConnectionState.Idle }
    }

    override fun onDbChanged(tables: List<String>) {
        _dbChanged.tryEmit(tables.toSet())
    }

    // Redundant with the doorbell (each maps to a DB write) but funneled in too, so a write path that
    // ever bypasses the hook still triggers a re-read. No persistent state trusts the payload — the DB
    // is re-read. A received message is also surfaced typed, for the (transient) push notification.
    override fun onMessage(event: MessageEvent) {
        _dbChanged.tryEmit(MESSAGES)
        if (event is MessageEvent.Received) {
            _incoming.tryEmit(IncomingMessage(event.from.toHex(), event.content, event.timestamp.toLong() * 1000))
        }
    }

    override fun onReaction(
        peer: ByteArray, dispatchId: ByteArray, reactor: ByteArray, emoji: String, add: Boolean,
    ) {
        _dbChanged.tryEmit(REACTIONS)
    }

    override fun onActivity(peer: ByteArray, activity: UShort) {
        _activity.tryEmit(ActivitySignal(peer, activity.toInt()))
    }

    override fun onPresence(peer: ByteArray, presence: FfiPresence) {
        val p = when (presence) {
            is FfiPresence.Online -> Presence.Online
            is FfiPresence.Idle -> Presence.Idle(presence.since.toLong())
            is FfiPresence.Offline ->
                if (presence.lastSeen == 0uL) Presence.Unknown else Presence.LastSeen(presence.lastSeen.toLong())
        }
        _presence.tryEmit(PresenceSignal(peer, p))
        _presenceByPeer.value = _presenceByPeer.value + (peer.toHex() to p)
    }

    private fun <T> bounded(): MutableSharedFlow<T> = MutableSharedFlow(
        extraBufferCapacity = 64,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    private val MESSAGES = setOf("messages")
    private val REACTIONS = setOf("reactions")
}
