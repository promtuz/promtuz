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
import timber.log.Timber
import uniffi.core.CoreEvents
import uniffi.core.MessageEvent
import uniffi.core.ConnectionState as FfiConnectionState
import uniffi.core.Presence as FfiPresence

/** [bits] is an OR of Activity bits; 0 is idle. */
class ActivitySignal(val conversation: ByteArray, val peer: ByteArray, val bits: Int)

class PresenceSignal(val peer: ByteArray, val presence: Presence)

/** Core calls these on its own threads. They must not block, and on_db_changed runs with the writer
 *  connection locked, so each body is a tryEmit into a bounded flow or a guarded call. */
object CoreEventBus : CoreEvents {
    private val _connection = MutableStateFlow(ConnectionState.Idle)
    val connection: StateFlow<ConnectionState> = _connection.asStateFlow()

    private val _dbChanged = bounded<Set<String>>()
    val dbChanged: SharedFlow<Set<String>> = _dbChanged.asSharedFlow()

    private val _activity = bounded<ActivitySignal>()
    val activity: SharedFlow<ActivitySignal> = _activity.asSharedFlow()

    private val _presence = bounded<PresenceSignal>()
    val presence: SharedFlow<PresenceSignal> = _presence.asSharedFlow()

    /** Keyed by hex IPK. The event stream has no memory, so late collectors start here. */
    private val _presenceByPeer = MutableStateFlow<Map<String, Presence>>(emptyMap())
    val presenceByPeer: StateFlow<Map<String, Presence>> = _presenceByPeer.asStateFlow()

    fun hydratePresence(seed: Map<String, Presence>) {
        if (seed.isNotEmpty()) _presenceByPeer.value = seed
    }

    override fun onConnection(state: FfiConnectionState) {
        _connection.value = when (state) {
            FfiConnectionState.DISCONNECTED -> ConnectionState.Disconnected
            FfiConnectionState.RESOLVING -> ConnectionState.Resolving
            FfiConnectionState.CONNECTING -> ConnectionState.Connecting
            FfiConnectionState.HANDSHAKING -> ConnectionState.Handshaking
            FfiConnectionState.CONNECTED -> ConnectionState.Connected
            FfiConnectionState.FAILED -> ConnectionState.Failed
            FfiConnectionState.SYNCING -> ConnectionState.Syncing
        }
    }

    override fun onDbChanged(tables: List<String>) {
        _dbChanged.tryEmit(tables.toSet())
    }

    override fun onMessage(event: MessageEvent) {
        _dbChanged.tryEmit(MESSAGES)
    }

    override fun onReaction(
        conversation: ByteArray, dispatchId: ByteArray, reactor: ByteArray, emoji: String,
        add: Boolean,
    ) {
        _dbChanged.tryEmit(REACTIONS)
    }

    override fun onCall(event: uniffi.core.CallEvent) = guard {
        // A call writes a message row when it ends.
        com.promtuz.core.call.CallController.onEvent(event)
        if (event is uniffi.core.CallEvent.Ended) _dbChanged.tryEmit(MESSAGES)
    }

    override fun onCallVideo(frame: ByteArray, keyframe: Boolean) = guard {
        com.promtuz.core.call.CallVideoManager.onFrame(frame, keyframe)
    }

    override fun onCallVideoKeyframe() = guard {
        com.promtuz.core.call.CallVideoManager.onKeyframeNeeded()
    }

    override fun onCallVideoBitrate(kbps: UInt) = guard {
        com.promtuz.core.call.CallVideoManager.onBitrate(kbps.toInt())
    }

    override fun onActivity(conversation: ByteArray, peer: ByteArray, activity: UShort) {
        _activity.tryEmit(ActivitySignal(conversation, peer, activity.toInt()))
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

    private inline fun guard(block: () -> Unit) {
        try {
            block()
        } catch (t: Throwable) {
            Timber.tag("Core").e(t, "event handler failed")
        }
    }

    private fun <T> bounded(): MutableSharedFlow<T> = MutableSharedFlow(
        extraBufferCapacity = 64,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    private val MESSAGES = setOf("messages")
    private val REACTIONS = setOf("reactions")
}
