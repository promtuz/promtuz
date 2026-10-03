package com.promtuz.core.call

import android.content.Context
import android.view.Surface
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

object CallVideoManager {
    private lateinit var app: Context

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

    fun start() {
        if (active) return
        active = true
        _cameraOn.value = true
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
        video = CallVideo(app).also { it.start(localSurface) }
    }

    private fun startDecoder() {
        val surface = remoteSurface ?: return
        if (!active || decoder != null) return
        decoder = CallVideoDecoder(surface).also { it.start() }
    }
}
