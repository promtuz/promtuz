package com.promtuz.core.call

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioManager
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.IBinder
import com.promtuz.core.call.CallController.Phase
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import timber.log.Timber

/**
 * Holds a call alive: the foreground notification with the microphone type,
 * the audio device, and a watch on the default network so a wifi-to-cellular
 * switch restarts ICE. Runs from the first call event to the last; the audio
 * device itself only runs while media is flowing.
 */
class CallService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var audio: CallAudio
    private var audioRunning = false
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        audio = CallAudio(getSystemService(AUDIO_SERVICE) as AudioManager)
        watchNetwork()
        // Post the foreground notification at once, from current state, so the
        // service is promoted within the OS window even before the first
        // collect tick.
        val state = CallController.state.value
        startForeground(
            CallNotifications.ONGOING_ID,
            if (state != null && !state.outgoing && state.phase == Phase.Incoming)
                CallNotifications.build(this, state, ringing = true)
            else CallNotifications.build(this, state, ringing = false),
        )
        scope.launch {
            CallController.state.collectLatest { ui ->
                if (ui == null) {
                    stopSelf()
                    return@collectLatest
                }
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

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Foreground already posted in onCreate; the type is declared here for
        // Android 14+, which wants it named at promotion.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            val state = CallController.state.value
            var type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            if (state?.video == true) type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
            runCatching {
                startForeground(
                    CallNotifications.ONGOING_ID,
                    CallNotifications.build(this, state, ringing = state?.phase == Phase.Incoming),
                    type,
                )
            }.onFailure { Timber.tag("Call").w(it, "call foreground type refused") }
        }
        return START_NOT_STICKY
    }

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
