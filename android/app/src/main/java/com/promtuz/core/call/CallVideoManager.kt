package com.promtuz.core.call

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import androidx.core.content.ContextCompat
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

object CallVideoManager {
    private lateinit var app: Context

    /** Shared by every capture, so an old camera's close runs before the next one opens. */
    private val cameraThread by lazy { HandlerThread("call-camera").apply { start() } }
    private var video: CallVideo? = null
    private var decoder: CallVideoDecoder? = null
    private var localSurface: Surface? = null
    private var remoteSurface: Surface? = null
    private var active = false

    private val _cameraOn = MutableStateFlow(true)
    val cameraOn: StateFlow<Boolean> = _cameraOn.asStateFlow()

    fun init(context: Context) {
        app = context.applicationContext
    }

    /** The peer hears that our camera is off, so they show our avatar instead of a black frame. */
    fun start(camera: Boolean) {
        if (active) return
        active = true
        val on = camera && ContextCompat.checkSelfPermission(app, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED
        _cameraOn.value = on
        if (!on) CoreBridge.callSetCamera(false)
        startEncoder()
        startDecoder()
    }

    fun stop() {
        active = false
        video?.stop()
        video = null
        decoder?.stop()
        decoder = null
        localSurface = null
        remoteSurface = null
    }

    fun setLocalSurface(surface: Surface?) {
        localSurface = surface
        if (active && _cameraOn.value) {
            // Recreates the capture session so the self-view is one of its targets.
            video?.stop()
            video = null
            startEncoder()
        }
    }

    fun setRemoteSurface(surface: Surface?) {
        remoteSurface = surface
        decoder?.stop()
        decoder = null
        if (active) startDecoder()
    }

    fun toggleCamera() {
        val on = !_cameraOn.value
        _cameraOn.value = on
        CoreBridge.callSetCamera(on)
        if (on) startEncoder() else {
            video?.stop()
            video = null
        }
    }

    fun switchCamera() {
        video?.switchCamera()
    }

    fun onFrame(frame: ByteArray, keyframe: Boolean) {
        decoder?.submit(frame, keyframe)
    }

    fun onKeyframeNeeded() {
        video?.requestKeyframe()
    }

    fun onBitrate(kbps: Int) {
        video?.setBitrate(kbps)
    }

    private fun startEncoder() {
        if (!active || !_cameraOn.value || video != null) return
        video = CallVideo(app, Handler(cameraThread.looper)).also { it.start(localSurface) }
    }

    private fun startDecoder() {
        val surface = remoteSurface ?: return
        if (!active || decoder != null) return
        decoder = CallVideoDecoder(surface).also { it.start() }
    }
}
