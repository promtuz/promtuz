package com.promtuz.chat.ui.camera

import android.content.Context
import android.view.ViewGroup
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageCapture
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.lifecycle.awaitInstance
import androidx.camera.video.FallbackStrategy
import androidx.camera.video.Quality
import androidx.camera.video.QualitySelector
import androidx.camera.video.Recorder
import androidx.camera.video.VideoCapture
import androidx.camera.view.PreviewView
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.LifecycleOwner

/**
 * One camera for the whole app. The attach tile and the full camera share the same
 * [PreviewView] and the same bound use cases, so opening the camera from the tile is
 * a reparent of a view that is already streaming, not a second warm-up and a black frame.
 */
object CameraSession {
    var camera by mutableStateOf<Camera?>(null)
        private set
    var front by mutableStateOf(false)
        private set
    var zoomRatio by mutableStateOf(1f)
    var zoomLinear by mutableStateOf(0f)

    private var view: PreviewView? = null
    private var provider: ProcessCameraProvider? = null
    private var boundTo: LifecycleOwner? = null
    private val preview = Preview.Builder().build()
    val imageCapture: ImageCapture = ImageCapture.Builder().setCaptureMode(ImageCapture.CAPTURE_MODE_MINIMIZE_LATENCY).build()
    val videoCapture: VideoCapture<Recorder> = VideoCapture.withOutput(
        Recorder.Builder().setQualitySelector(
            QualitySelector.fromOrderedList(listOf(Quality.FHD, Quality.HD, Quality.SD), FallbackStrategy.lowerQualityOrHigherThan(Quality.SD)),
        ).build(),
    )

    /** The shared view, pulled out of whatever parent last held it. */
    fun previewView(context: Context): PreviewView {
        val v = view ?: PreviewView(context.applicationContext).apply {
            scaleType = PreviewView.ScaleType.FILL_CENTER
            preview.surfaceProvider = surfaceProvider
        }.also { view = it }
        (v.parent as? ViewGroup)?.removeView(v)
        return v
    }

    /** Binds every use case once per lifecycle owner; a second caller with the same owner is a no-op. */
    suspend fun bind(context: Context, owner: LifecycleOwner, rebind: Boolean = false) {
        val p = provider ?: ProcessCameraProvider.awaitInstance(context.applicationContext).also { provider = it }
        if (!rebind && boundTo === owner && camera != null) return
        val selector = if (front) CameraSelector.DEFAULT_FRONT_CAMERA else CameraSelector.DEFAULT_BACK_CAMERA
        p.unbindAll()
        camera = runCatching { p.bindToLifecycle(owner, selector, preview, imageCapture, videoCapture) }
            .getOrElse { runCatching { p.bindToLifecycle(owner, selector, preview, imageCapture) }.getOrNull() }
        boundTo = owner
        zoomRatio = 1f
        zoomLinear = 0f
    }

    suspend fun flip(context: Context, owner: LifecycleOwner) {
        front = !front
        bind(context, owner, rebind = true)
    }

    fun zoomToLinear(linear: Float) {
        val c = camera ?: return
        zoomLinear = linear.coerceIn(0f, 1f)
        c.cameraControl.setLinearZoom(zoomLinear)
        c.cameraInfo.zoomState.value?.let { zs ->
            zoomRatio = zs.minZoomRatio + (zs.maxZoomRatio - zs.minZoomRatio) * zoomLinear
        }
    }

    fun zoomToRatio(ratio: Float) {
        val c = camera ?: return
        val zs = c.cameraInfo.zoomState.value ?: return
        zoomRatio = ratio.coerceIn(zs.minZoomRatio, zs.maxZoomRatio)
        zoomLinear = if (zs.maxZoomRatio > zs.minZoomRatio) (zoomRatio - zs.minZoomRatio) / (zs.maxZoomRatio - zs.minZoomRatio) else 0f
        c.cameraControl.setZoomRatio(zoomRatio)
    }

    /** Lets the camera go when nothing shows it any more. */
    fun release() {
        provider?.unbindAll()
        camera = null
        boundTo = null
    }
}
