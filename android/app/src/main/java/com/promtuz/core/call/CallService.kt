package com.promtuz.core.call

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
import androidx.core.content.ContextCompat
import com.promtuz.core.call.CallController.Phase
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
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
        // Promote at once, from current state, so the service is foreground
        // within the OS window even before the first collect tick.
        promote()
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
        promote()
        return START_NOT_STICKY
    }

    /** Post the foreground notification with a capture type that matches the
     *  call and the permissions actually held. The service starts only after a
     *  call is answered or placed, so the type it needs is always granted; an
     *  audio call promotes with microphone alone, never the camera it lacks. */
    private fun promote() {
        val state = CallController.state.value
        val ringing = state != null && !state.outgoing && state.phase == Phase.Incoming
        val notif = CallNotifications.build(this, state, ringing = ringing)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val type = captureType(state)
            runCatching {
                if (type != 0) startForeground(CallNotifications.ONGOING_ID, notif, type)
                else startForeground(CallNotifications.ONGOING_ID, notif)
            }.onFailure { Timber.tag("Call").w(it, "call foreground failed") }
        } else {
            startForeground(CallNotifications.ONGOING_ID, notif)
        }
    }

    private fun captureType(state: CallController.Ui?): Int {
        var type = 0
        if (granted(android.Manifest.permission.RECORD_AUDIO)) {
            type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        }
        if (state?.video == true &&
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
            granted(android.Manifest.permission.CAMERA)
        ) {
            type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        }
        return type
    }

    private fun granted(permission: String) =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    override fun onDestroy() {
        // Cancel the state collector; without this a destroyed instance keeps
        // observing and can restart its old audio device alongside a new call.
        scope.cancel()
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
