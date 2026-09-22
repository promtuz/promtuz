package com.promtuz.core.call

import android.content.Context
import android.content.Intent
import android.os.Build
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import timber.log.Timber
import uniffi.core.CallEndReason
import uniffi.core.CallEvent

/**
 * The one place the app tracks a call. It turns core's `on_call` events into a
 * single observable state for the UI, runs the foreground service that holds
 * the microphone and the ringing notification, and remembers the last call so
 * a missed-call notice can be shown after it ends.
 *
 * One call at a time, mirroring the engine. Everything the UI does to a call it
 * does through [CoreBridge]; everything it reads it reads from [state].
 */
object CallController {
    /** A call shaped for the screen. */
    data class Ui(
        val callId: ByteArray,
        val peer: ByteArray,
        val conversation: ByteArray,
        val name: String,
        val outgoing: Boolean,
        val video: Boolean,
        val phase: Phase,
        val muted: Boolean,
        val peerMuted: Boolean,
        val peerCamera: Boolean,
        val speaker: Boolean,
        /** When the call connected, elapsed-real-time millis, for the timer. */
        val connectedAt: Long,
    )

    enum class Phase { Outgoing, Incoming, Ringing, Connecting, Connected, Reconnecting }

    private val _state = MutableStateFlow<Ui?>(null)
    val state: StateFlow<Ui?> = _state.asStateFlow()

    private lateinit var app: Context

    fun init(context: Context) {
        app = context.applicationContext
    }

    /** Called from [com.promtuz.core.adapter.CoreEventBus.onCall] on a core thread. */
    fun onEvent(event: CallEvent) {
        when (event) {
            is CallEvent.Outgoing -> begin(event.call, event.peer, event.conversation, outgoing = true, video = videoNow(event.call), Phase.Outgoing)
            is CallEvent.Incoming -> begin(event.call, event.peer, event.conversation, outgoing = false, video = event.video, Phase.Incoming)
            is CallEvent.Ringing -> update(event.call) { it.copy(phase = Phase.Ringing) }
            is CallEvent.Connecting -> update(event.call) { it.copy(phase = Phase.Connecting) }
            is CallEvent.Connected -> update(event.call) {
                if (it.video) CallVideoManager.start()
                it.copy(phase = Phase.Connected, connectedAt = android.os.SystemClock.elapsedRealtime())
            }
            is CallEvent.Reconnecting -> update(event.call) { it.copy(phase = Phase.Reconnecting) }
            is CallEvent.PeerMuted -> update(event.call) { it.copy(peerMuted = event.muted) }
            is CallEvent.PeerCamera -> update(event.call) { it.copy(peerCamera = event.on) }
            is CallEvent.Ended -> ended(event)
        }
    }

    /** An outgoing call's video flag, read from core's current-call snapshot. */
    private fun videoNow(call: ByteArray): Boolean =
        runCatching { CoreBridge.callCurrent()?.takeIf { it.call.contentEquals(call) }?.video }
            .getOrNull() ?: false

    private fun begin(
        call: ByteArray, peer: ByteArray, conversation: ByteArray, outgoing: Boolean, video: Boolean,
        phase: Phase,
    ) {
        val name = runCatching { CoreBridge.contactName(peer) }.getOrNull().orEmpty()
        _state.value = Ui(call, peer, conversation, name, outgoing, video, phase, false, false, true, false, 0)
        startService()
        if (!outgoing) CallNotifications.ringing(app, _state.value!!)
        CallActivity.launch(app)
    }

    private fun update(call: ByteArray, f: (Ui) -> Ui) {
        val current = _state.value ?: return
        if (!current.callId.contentEquals(call)) return
        val next = f(current)
        _state.value = next
        // Once connected the ringing notification becomes the ongoing one.
        if (next.phase == Phase.Connected || next.phase == Phase.Reconnecting) {
            CallNotifications.ongoing(app, next)
        }
    }

    private fun ended(event: CallEvent.Ended) {
        val current = _state.value
        if (current != null && !current.callId.contentEquals(event.call)) return
        _state.value = null
        CallVideoManager.stop()
        stopService()
        CallNotifications.clearOngoing(app)
        if (event.reason == CallEndReason.MISSED) {
            val name = runCatching { CoreBridge.contactName(event.peer) }.getOrNull().orEmpty()
            CallNotifications.missed(app, event.call, event.conversation, name)
        }
    }

    // — UI actions (each just delegates; state moves on the next core event) —

    fun toggleMute() {
        val s = _state.value ?: return
        CoreBridge.callSetMuted(!s.muted)
        _state.value = s.copy(muted = !s.muted)
    }

    fun toggleSpeaker() {
        val s = _state.value ?: return
        val on = !s.speaker
        CallService.instance?.setSpeaker(on)
        _state.value = s.copy(speaker = on)
    }

    fun toggleCamera() = CallVideoManager.toggleCamera()

    fun switchCamera() = CallVideoManager.switchCamera()

    /** The default network moved; tell the engine to restart ICE now. */
    fun networkChanged() {
        if (_state.value != null) CoreBridge.callNetworkChanged()
    }

    private fun startService() {
        val intent = Intent(app, CallService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) app.startForegroundService(intent)
        else app.startService(intent)
    }

    private fun stopService() {
        runCatching { app.stopService(Intent(app, CallService::class.java)) }
            .onFailure { Timber.tag("Call").w(it, "stopService failed") }
    }
}
