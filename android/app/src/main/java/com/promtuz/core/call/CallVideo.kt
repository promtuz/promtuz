package com.promtuz.core.call

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.ImageFormat
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import androidx.core.content.ContextCompat
import com.promtuz.core.CoreBridge
import timber.log.Timber
import java.nio.ByteBuffer

/**
 * The camera and the H.264 encoder for a video call. One Camera2 capture
 * request feeds two surfaces at once, the encoder's input and the local
 * self-view, so there is no GL copy. Encoded Annex-B access units go to core;
 * core drives the target bitrate and asks for keyframes.
 *
 * libcore owns the RTP and the network; this owns only pixels to bytes.
 */
class CallVideo(private val context: Context) {
    private companion object {
        const val WIDTH = 640
        const val HEIGHT = 480
        const val FPS = 30
        const val START_BITRATE = 600_000
        const val KEYFRAME_SECONDS = 2
    }

    private val cameraManager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
    private val thread = HandlerThread("call-camera").apply { start() }
    private val handler = Handler(thread.looper)

    private var encoder: MediaCodec? = null
    private var encoderSurface: Surface? = null
    private var camera: CameraDevice? = null
    private var previewSurface: Surface? = null
    private var frontFacing = true
    @Volatile private var running = false
    /// SPS/PPS from the encoder, prepended to every keyframe so a peer that
    /// missed the first ones can still start decoding after a loss.
    private var codecConfig: ByteArray? = null

    /** Start capture, rendering the local preview into [preview]. */
    fun start(preview: Surface?) {
        if (running) return
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA)
            != PackageManager.PERMISSION_GRANTED
        ) {
            Timber.tag("Call").w("camera permission not granted; video capture skipped")
            return
        }
        running = true
        previewSurface = preview
        handler.post { startEncoder(); openCamera() }
    }

    fun stop() {
        if (running) {
            running = false
            handler.post {
                runCatching { camera?.close() }
                camera = null
                runCatching { encoder?.stop() }
                runCatching { encoder?.release() }
                encoder = null
                encoderSurface?.release()
                encoderSurface = null
            }
        }
        // Always quit the thread: the constructor started it, so even a start()
        // that bailed on a denied permission must not leak it. quitSafely lets
        // the cleanup post above run first.
        thread.quitSafely()
    }

    /** Flip between the front and back camera. */
    fun switchCamera() {
        if (!running) return
        handler.post {
            frontFacing = !frontFacing
            runCatching { camera?.close() }
            camera = null
            openCamera()
        }
    }

    /** The peer (via core) asked for a fresh IDR. */
    fun requestKeyframe() {
        val bundle = Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) }
        runCatching { encoder?.setParameters(bundle) }
    }

    /** Retune the encoder to the bandwidth estimate. */
    fun setBitrate(kbps: Int) {
        val bundle = Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, kbps * 1000) }
        runCatching { encoder?.setParameters(bundle) }
    }

    private fun startEncoder() {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, WIDTH, HEIGHT).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, START_BITRATE)
            setInteger(MediaFormat.KEY_FRAME_RATE, FPS)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, KEYFRAME_SECONDS)
            // Constrained baseline: the floor the spec picks so every phone decodes it.
            setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileConstrainedBaseline)
            setInteger("bitrate-mode", MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {}
            override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                val buffer = codec.getOutputBuffer(index)
                if (buffer != null) emit(buffer, info)
                runCatching { codec.releaseOutputBuffer(index, false) }
            }
            override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                Timber.tag("Call").w(e, "video encoder error")
            }
            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
        }, handler)
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        encoderSurface = codec.createInputSurface()
        codec.start()
        encoder = codec
    }

    private fun emit(buffer: ByteBuffer, info: MediaCodec.BufferInfo) {
        val bytes = ByteArray(info.size)
        buffer.position(info.offset)
        buffer.get(bytes)
        if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
            // SPS/PPS: keep it, don't send it alone.
            codecConfig = bytes
            return
        }
        val keyframe = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
        val frame = if (keyframe) codecConfig?.let { it + bytes } ?: bytes else bytes
        CoreBridge.callPushVideo(frame, keyframe)
    }

    private fun openCamera() {
        val id = pickCamera() ?: return
        try {
            cameraManager.openCamera(id, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    camera = device
                    if (running) startSession(device) else device.close()
                }
                override fun onDisconnected(device: CameraDevice) {
                    device.close()
                    if (camera === device) camera = null
                }
                override fun onError(device: CameraDevice, error: Int) {
                    Timber.tag("Call").w("camera error $error")
                    device.close()
                }
            }, handler)
        } catch (e: SecurityException) {
            Timber.tag("Call").w(e, "camera open denied")
        }
    }

    private fun startSession(device: CameraDevice) {
        val encoderSurface = encoderSurface ?: return
        val targets = listOfNotNull(encoderSurface, previewSurface)
        val request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
            targets.forEach { addTarget(it) }
            set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
        }
        @Suppress("DEPRECATION")
        device.createCaptureSession(targets, object : android.hardware.camera2.CameraCaptureSession.StateCallback() {
            override fun onConfigured(session: android.hardware.camera2.CameraCaptureSession) {
                if (!running) return
                runCatching { session.setRepeatingRequest(request.build(), null, handler) }
            }
            override fun onConfigureFailed(session: android.hardware.camera2.CameraCaptureSession) {
                Timber.tag("Call").w("camera session config failed")
            }
        }, handler)
    }

    private fun pickCamera(): String? {
        val want = if (frontFacing) CameraCharacteristics.LENS_FACING_FRONT
        else CameraCharacteristics.LENS_FACING_BACK
        val ids = runCatching { cameraManager.cameraIdList }.getOrNull() ?: return null
        return ids.firstOrNull {
            cameraManager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == want
        } ?: ids.firstOrNull()
    }
}
