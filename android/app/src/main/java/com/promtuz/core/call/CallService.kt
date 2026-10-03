package com.promtuz.core.call

import android.Manifest
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.AudioManager
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.IBinder
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import com.promtuz.core.call.CallController.Phase
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import timber.log.Timber

/** Runs from the first call event to the last; the audio device runs only while media flows. */
class CallService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var audio: CallAudio
    private var audioRunning = false
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var promotedType = 0

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        audio = CallAudio(getSystemService(AUDIO_SERVICE) as AudioManager)
        watchNetwork()
        // Promote inside the OS window, before the first collect tick.
        promote(CallController.state.value)
        scope.launch {
            CallController.state.collectLatest { ui ->
                if (ui == null) {
                    stopSelf()
                    return@collectLatest
                }
                promote(ui)
                val flowing = ui.phase == Phase.Connected || ui.phase == Phase.Reconnecting
                if (flowing && !audioRunning) {
                    audio.start()
                    audioRunning = true
                } else if (!flowing && audioRunning) {
                    audio.stop()
                    audioRunning = false
                }
            }
        }
    }

    fun setSpeaker(on: Boolean) {
        audio.speaker = on
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_NOT_STICKY

    // Microphone and camera are while-in-use types: a ring from the background
    // carries neither, and the answer tap adds what the granted permissions allow.
    private fun promote(state: CallController.Ui?) {
        val ringing = state != null && !state.outgoing && state.phase == Phase.Incoming
        var type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
        if (!ringing) {
            if (granted(Manifest.permission.RECORD_AUDIO)) type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            if (state?.video == true && granted(Manifest.permission.CAMERA)) type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        }
        if (type == promotedType) return
        runCatching {
            ServiceCompat.startForeground(this, CallNotifications.ONGOING_ID, CallNotifications.build(this, state, ringing), type)
            promotedType = type
        }.onFailure { Timber.tag("Call").w(it, "call foreground promotion refused") }
    }

    private fun granted(permission: String) =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    override fun onDestroy() {
        if (audioRunning) audio.stop()
        unwatchNetwork()
        instance = null
        super.onDestroy()
    }

    private fun watchNetwork() {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) = CallController.networkChanged()
            override fun onLost(network: Network) = CallController.networkChanged()
        }
        runCatching { cm.registerDefaultNetworkCallback(cb) }
            .onSuccess { networkCallback = cb }
            .onFailure { Timber.tag("Call").w(it, "network watch failed") }
    }

    private fun unwatchNetwork() {
        val cb = networkCallback ?: return
        val cm = getSystemService(ConnectivityManager::class.java)
        runCatching { cm?.unregisterNetworkCallback(cb) }
        networkCallback = null
    }

    companion object {
        @Volatile
        var instance: CallService? = null
            private set
    }
}
