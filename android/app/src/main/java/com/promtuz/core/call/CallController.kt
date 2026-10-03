package com.promtuz.core.call

import android.content.Context
import android.content.Intent
import android.os.Build
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.flow.updateAndGet
import timber.log.Timber
import uniffi.core.CallEndReason
import uniffi.core.CallEvent

/** One call at a time, as in core: its events become [state], and the service and notifications follow. */
object CallController {
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
        /** SystemClock.elapsedRealtime at the first connect; 0 until then. */
        val connectedAt: Long,
        /** Whether our camera starts with the video; an audio call that crossed into a video one keeps it off. */
        val camera: Boolean = video,
    )

    enum class Phase { Outgoing, Incoming, Ringing, Connecting, Connected, Reconnecting }

    private val _state = MutableStateFlow<Ui?>(null)
    val state: StateFlow<Ui?> = _state.asStateFlow()

    private val _answering = MutableStateFlow<ByteArray?>(null)
    /** The call an Answer tap picked up; the call screen gets the permissions, then [answer]s it. */
    val answering: StateFlow<ByteArray?> = _answering.asStateFlow()

    private lateinit var app: Context

    fun init(context: Context) {
        app = context.applicationContext
    }

    /** Runs on a core thread, from [com.promtuz.core.adapter.CoreEventBus.onCall]. */
    fun onEvent(event: CallEvent) {
        when (event) {
            is CallEvent.Outgoing -> begin(event.call, event.peer, event.conversation, outgoing = true, video = videoNow(event.call), Phase.Outgoing)
            is CallEvent.Incoming -> begin(event.call, event.peer, event.conversation, outgoing = false, video = event.video, Phase.Incoming)
            is CallEvent.Ringing -> update(event.call) { it.copy(phase = Phase.Ringing) }
            is CallEvent.Switched -> update(event.from) {
                it.copy(callId = event.to, outgoing = false, video = event.video, camera = it.video, phase = Phase.Connecting)
            }
            is CallEvent.Connecting -> update(event.call) { it.copy(phase = Phase.Connecting) }
            is CallEvent.Connected -> {
                // A reconnect connects again; the timer keeps its first start.
                val now = android.os.SystemClock.elapsedRealtime()
                update(event.call) {
                    it.copy(phase = Phase.Connected, connectedAt = it.connectedAt.takeIf { t -> t != 0L } ?: now)
                }
                _state.value?.takeIf { it.video }?.let { CallVideoManager.start(it.camera) }
            }
            is CallEvent.Reconnecting -> update(event.call) { it.copy(phase = Phase.Reconnecting) }
            is CallEvent.PeerMuted -> update(event.call) { it.copy(peerMuted = event.muted) }
            is CallEvent.PeerCamera -> update(event.call) { it.copy(peerCamera = event.on) }
            is CallEvent.Ended -> ended(event)
        }
    }

    private fun videoNow(call: ByteArray): Boolean =
        runCatching { CoreBridge.callCurrent()?.takeIf { it.call.contentEquals(call) }?.video }
            .getOrNull() ?: false

    private fun begin(
        call: ByteArray, peer: ByteArray, conversation: ByteArray, outgoing: Boolean, video: Boolean,
        phase: Phase,
    ) {
        val name = runCatching { CoreBridge.contactName(peer) }.getOrNull().orEmpty()
        _state.value = Ui(call, peer, conversation, name, outgoing, video, phase, false, false, true, false, 0)
        // A ring holds no capture service; answering gets the microphone first, then starts it.
        if (outgoing) startService() else CallNotifications.ringing(app, _state.value!!)
        CallActivity.launch(app)
    }

    /** Events land from several threads, so the state moves in one CAS step. */
    private fun update(call: ByteArray, f: (Ui) -> Ui) {
        var next: Ui? = null
        _state.update { s -> if (s != null && s.callId.contentEquals(call)) f(s).also { next = it } else s }
        val ui = next ?: return
        // Once answered the ringing notification becomes the ongoing one, so a
        // second Answer tap has nothing to press.
        if (ui.phase == Phase.Connecting || ui.phase == Phase.Connected || ui.phase == Phase.Reconnecting) {
            CallNotifications.ongoing(app, ui)
        }
    }

    private fun ended(event: CallEvent.Ended) {
        // An expired offer that never became the current call is still missed.
        if (event.reason == CallEndReason.MISSED) {
            val name = runCatching { CoreBridge.contactName(event.peer) }.getOrNull().orEmpty()
            CallNotifications.missed(app, event.conversation, name)
        }
        val current = _state.value
        if (current != null && !current.callId.contentEquals(event.call)) return
        _state.value = null
        _answering.value = null
        CallVideoManager.stop()
        stopService()
        CallNotifications.clearOngoing(app)
    }

    /** Ignored unless [call] is the current one, so a stale tap cannot prompt for the next call. */
    fun requestAnswer(call: ByteArray) {
        _state.value?.callId?.takeIf { it.contentEquals(call) }?.let { _answering.value = it }
    }

    fun cancelAnswer() {
        _answering.value = null
    }

    /** Core refuses unless [call] is still the one ringing, so a stale tap answers nothing. */
    fun answer(call: ByteArray) {
        _answering.value = null
        runCatching { CoreBridge.callAccept(call) }
            .onSuccess { startService() }
            .onFailure { Timber.tag("Call").w(it, "answer refused") }
    }

    fun toggleMute() {
        val s = _state.updateAndGet { it?.copy(muted = !it.muted) } ?: return
        CoreBridge.callSetMuted(s.muted)
    }

    fun toggleSpeaker() {
        val s = _state.updateAndGet { it?.copy(speaker = !it.speaker) } ?: return
        CallService.instance?.setSpeaker(s.speaker)
    }

    fun toggleCamera() = CallVideoManager.toggleCamera()

    fun switchCamera() = CallVideoManager.switchCamera()

    fun networkChanged() {
        if (_state.value != null) CoreBridge.callNetworkChanged()
    }

    private fun startService() {
        val intent = Intent(app, CallService::class.java)
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) app.startForegroundService(intent)
            else app.startService(intent)
        }.onFailure { Timber.tag("Call").w(it, "call service refused") }
    }

    private fun stopService() {
        runCatching { app.stopService(Intent(app, CallService::class.java)) }
            .onFailure { Timber.tag("Call").w(it, "stopService failed") }
    }
}
