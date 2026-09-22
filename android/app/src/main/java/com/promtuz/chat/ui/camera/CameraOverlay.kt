package com.promtuz.chat.ui.camera

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageCapture
import androidx.camera.core.ImageCaptureException
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.lifecycle.awaitInstance
import androidx.camera.video.FileOutputOptions
import androidx.camera.video.Quality
import androidx.camera.video.QualitySelector
import androidx.camera.video.Recorder
import androidx.camera.video.Recording
import androidx.camera.video.VideoCapture
import androidx.camera.video.VideoRecordEvent
import androidx.camera.view.PreviewView
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.geometry.lerp
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.util.lerp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.clock
import com.promtuz.chat.ui.components.LottieFrame
import com.promtuz.chat.ui.components.LottieLoop
import com.promtuz.chat.ui.components.rememberLottie
import androidx.compose.animation.core.LinearEasing
import androidx.compose.foundation.Image
import androidx.compose.ui.res.painterResource
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import java.io.File
import kotlin.math.abs

private const val OPEN_MS = 280
private val ease = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)
private val ZOOM_TRAVEL = 200.dp
private val LOCK_TRAVEL = 110.dp
private const val MAX_CLIP_MS = 5 * 60_000L

private enum class Flash { Off, On, Auto }

/** Mount once above navigation, after the media viewer. */
@Composable
fun CameraOverlayHost() {
    val request = CameraLauncher.request ?: return
    key(request) { CameraScreen(request) }
}

@Composable
private fun CameraScreen(request: CameraRequest) {
    val context = LocalContext.current
    val density = LocalDensity.current
    val scope = rememberCoroutineScope()

    var viewport by remember { mutableStateOf(Size.Zero) }
    val hostCoords = remember { arrayOfNulls<LayoutCoordinates>(1) }
    val open = remember { Animatable(0f) }
    var from by remember { mutableStateOf<Rect?>(null) }
    var fromRadius by remember { mutableFloatStateOf(0f) }
    var closing by remember { mutableStateOf(false) }

    fun close() {
        if (closing) return
        closing = true
        scope.launch {
            val origin = MediaViewer.originRectIn(CameraLauncher.TILE_ORIGIN, hostCoords[0], density)
            from = origin?.first
            fromRadius = origin?.second ?: 0f
            open.animateTo(0f, tween(OPEN_MS, easing = ease))
            CameraLauncher.close()
        }
    }

    LaunchedEffect(Unit) {
        snapshotFlow { viewport }.first { it != Size.Zero }
        val origin = MediaViewer.originRectIn(CameraLauncher.TILE_ORIGIN, hostCoords[0], density)
        from = origin?.first
        fromRadius = origin?.second ?: 0f
        open.snapTo(0f)
        open.animateTo(1f, tween(OPEN_MS, easing = ease))
    }
    BackHandler { close() }

    Box(
        Modifier
            .fillMaxSize()
            .onSizeChanged { viewport = Size(it.width.toFloat(), it.height.toFloat()) }
            .onGloballyPositioned { hostCoords[0] = it }
            .drawBehind {
                val p = open.value
                val full = Rect(Offset.Zero, size)
                val start = from ?: Rect(full.center - Offset(full.width * 0.25f, full.height * 0.25f), full.size * 0.5f)
                val r = lerp(start, full, p)
                drawRoundRect(Color.Black, r.topLeft, r.size, CornerRadius(lerp(fromRadius, 0f, p)),
                    alpha = if (from != null) 1f else p)
            },
    ) {
        Box(Modifier.fillMaxSize().graphicsLayer { alpha = ((open.value - 0.4f) / 0.6f).coerceIn(0f, 1f) }) {
            CameraBody(request, ::close)
        }
    }
}

@Composable
private fun CameraBody(request: CameraRequest, onClose: () -> Unit) {
    val context = LocalContext.current
    var granted by remember {
        mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
    }
    var asked by remember { mutableStateOf(false) }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        granted = it
        asked = true
    }
    LaunchedEffect(Unit) { if (!granted) launcher.launch(Manifest.permission.CAMERA) }

    when {
        granted -> LiveCamera(request, onClose)
        asked -> Column(
            Modifier.fillMaxSize().padding(32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = androidx.compose.foundation.layout.Arrangement.spacedBy(12.dp, Alignment.CenterVertically),
        ) {
            Image(painterResource(R.drawable.ic_camera_permission), null, Modifier.size(96.dp).graphicsLayer { alpha = 0.6f })
            Text("Allow camera access to take photos here", color = Color.White,
                style = MaterialTheme.typography.titleMedium, textAlign = androidx.compose.ui.text.style.TextAlign.Center)
            Button({ launcher.launch(Manifest.permission.CAMERA) }) { Text("Allow") }
            CloseButton(onClose, Modifier)
        }
    }
}

@Composable
private fun LiveCamera(request: CameraRequest, onClose: () -> Unit) {
    val context = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    val density = LocalDensity.current

    val camera = CameraSession.camera
    val imageCapture = CameraSession.imageCapture
    val videoCapture = CameraSession.videoCapture
    val zoomRatio = CameraSession.zoomRatio
    var flash by remember { mutableStateOf(Flash.Off) }
    var recording by remember { mutableStateOf<Recording?>(null) }
    var recordingSince by remember { mutableLongStateOf(0L) }
    var elapsed by remember { mutableLongStateOf(0L) }
    var locked by remember { mutableStateOf(false) }
    var lockProgress by remember { mutableFloatStateOf(0f) }
    var pressed by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    val shutterFlash = remember { Animatable(0f) }

    // Usually already bound by the attach tile, in which case this returns at once.
    LaunchedEffect(Unit) { CameraSession.bind(context, owner) }
    fun flip() = scope.launch { CameraSession.flip(context, owner) }
    LaunchedEffect(flash) {
        imageCapture.flashMode = when (flash) {
            Flash.Off -> ImageCapture.FLASH_MODE_OFF
            Flash.On -> ImageCapture.FLASH_MODE_ON
            Flash.Auto -> ImageCapture.FLASH_MODE_AUTO
        }
    }
    LaunchedEffect(recording) {
        val r = recording ?: return@LaunchedEffect
        if (flash == Flash.On) camera?.cameraControl?.enableTorch(true)
        while (true) {
            elapsed = System.currentTimeMillis() - recordingSince
            if (elapsed >= MAX_CLIP_MS) { r.stop(); break }
            delay(250)
        }
    }

    fun setZoom(linear: Float) = CameraSession.zoomToLinear(linear)

    fun output(ext: String) = File(File(context.cacheDir, "attachments").apply { mkdirs() }, "${System.nanoTime()}_capture.$ext")

    fun takePhoto() {
        if (busy) return
        busy = true
        scope.launch { shutterFlash.snapTo(1f); shutterFlash.animateTo(0f, tween(220)) }
        val file = output("jpg")
        imageCapture.takePicture(
            ImageCapture.OutputFileOptions.Builder(file).build(),
            ContextCompat.getMainExecutor(context),
            object : ImageCapture.OnImageSavedCallback {
                override fun onImageSaved(outputFileResults: ImageCapture.OutputFileResults) {
                    request.onCaptured(file, false)
                    onClose()
                }
                override fun onError(exception: ImageCaptureException) {
                    busy = false
                    Toast.makeText(context, "Couldn’t take the photo", Toast.LENGTH_SHORT).show()
                }
            },
        )
    }

    fun startRecording() {
        if (busy || recording != null) return
        val file = output("mp4")
        val audio = ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        val pending = videoCapture.output.prepareRecording(context, FileOutputOptions.Builder(file).build())
        if (audio) pending.withAudioEnabled()
        recordingSince = System.currentTimeMillis()
        elapsed = 0L
        locked = false
        recording = pending.start(ContextCompat.getMainExecutor(context)) { event ->
            if (event is VideoRecordEvent.Finalize) {
                recording = null
                camera?.cameraControl?.enableTorch(false)
                if (!event.hasError() && file.length() > 0) {
                    request.onCaptured(file, true)
                    onClose()
                } else {
                    file.delete()
                    busy = false
                    if (event.hasError()) Toast.makeText(context, "Couldn’t record", Toast.LENGTH_SHORT).show()
                }
            }
        }
    }

    fun stopRecording() {
        busy = true
        recording?.stop()
    }

    val audioLauncher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {}
    LaunchedEffect(Unit) {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED)
            audioLauncher.launch(Manifest.permission.RECORD_AUDIO)
    }

    val touchSlop = LocalViewConfiguration.current.touchSlop
    val longPressMs = LocalViewConfiguration.current.longPressTimeoutMillis
    val zoomTravel = with(density) { ZOOM_TRAVEL.toPx() }
    val lockTravel = with(density) { LOCK_TRAVEL.toPx() }

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        AndroidView(
            factory = { CameraSession.previewView(it) },
            modifier = Modifier
                .fillMaxSize()
                .pointerInput(camera) {
                    awaitEachGesture {
                        awaitFirstDown(requireUnconsumed = false)
                        while (true) {
                            val event = awaitPointerEvent()
                            if (event.changes.count { it.pressed } >= 2) {
                                CameraSession.zoomToRatio(CameraSession.zoomRatio * event.calculateZoom())
                                event.changes.forEach { it.consume() }
                            }
                            if (event.changes.none { it.pressed }) break
                        }
                    }
                }
                .pointerInput(Unit) { detectTapGestures(onDoubleTap = { flip() }) },
        )

        // Photo flash.
        Box(Modifier.fillMaxSize().drawBehind { drawRect(Color.White, alpha = shutterFlash.value) })

        // Top row: close, timer, flash.
        Row(
            Modifier.fillMaxWidth().statusBarsPadding().height(56.dp).padding(horizontal = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CloseButton(onClose, Modifier.graphicsLayer { alpha = if (recording == null) 1f else 0f })
            Spacer(Modifier.weight(1f))
            if (recording != null) Row(
                Modifier.clip(RoundedCornerShape(14.dp)).background(Color.Black.copy(alpha = 0.45f))
                    .padding(horizontal = 10.dp, vertical = 5.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = androidx.compose.foundation.layout.Arrangement.spacedBy(6.dp),
            ) {
                LottieLoop(R.raw.camera_recording, Modifier.size(10.dp))
                Text(clock(elapsed), color = Color.White, style = MaterialTheme.typography.labelLarge, fontWeight = FontWeight.Medium)
            }
            Spacer(Modifier.weight(1f))
            Box(
                Modifier.size(44.dp).clip(CircleShape)
                    .graphicsLayer { alpha = if (recording == null) 1f else 0f }
                    .pointerInput(Unit) { detectTapGestures { flash = Flash.entries[(flash.ordinal + 1) % Flash.entries.size] } },
                contentAlignment = Alignment.Center,
            ) {
                DrawableIcon(
                    when (flash) { Flash.Off -> R.drawable.oi_flash_off; Flash.On -> R.drawable.oi_flash; Flash.Auto -> R.drawable.oi_flash_auto },
                    Modifier.size(24.dp),
                    desc = when (flash) { Flash.Off -> "Flash off"; Flash.On -> "Flash on"; Flash.Auto -> "Flash auto" },
                    tint = Color.White,
                )
            }
        }

        // Bottom: zoom chip, lock target, shutter, flip.
        Column(
            Modifier.align(Alignment.BottomCenter).fillMaxWidth().navigationBarsPadding().padding(bottom = 28.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Box(
                Modifier.clip(RoundedCornerShape(16.dp)).background(Color.Black.copy(alpha = 0.35f))
                    .padding(horizontal = 10.dp, vertical = 5.dp),
            ) {
                Text("%.1f×".format(zoomRatio), color = Color.White, style = MaterialTheme.typography.labelMedium)
            }
            Spacer(Modifier.height(18.dp))
            Box(Modifier.fillMaxWidth().height(96.dp)) {
                // Slide-to-lock target, left of the shutter, visible while holding.
                val lockAlpha by animateFloatAsState(if (recording != null) 1f else 0f, tween(160), label = "lock target")
                Box(
                    Modifier.align(Alignment.Center).graphicsLayer {
                        translationX = -lockTravel
                        alpha = lockAlpha
                        val s = 1f + 0.25f * lockProgress
                        scaleX = s; scaleY = s
                    }.size(32.dp),
                    contentAlignment = Alignment.Center,
                ) {
                    val lockFrame by animateFloatAsState(if (locked) 20f else 0f, tween(330, easing = LinearEasing), label = "lock")
                    LottieFrame(rememberLottie(R.raw.camera_lock), { lockFrame }, Modifier.size(32.dp))
                }

                Shutter(
                    recording = recording != null,
                    locked = locked,
                    pressed = pressed,
                    progress = { (elapsed.toFloat() / MAX_CLIP_MS).coerceIn(0f, 1f) },
                    modifier = Modifier.align(Alignment.Center).pointerInput(camera) {
                        awaitEachGesture {
                            val down = awaitFirstDown()
                            down.consume()
                            pressed = true
                            if (recording != null && locked) {
                                // Locked recording: a tap on the shutter ends it.
                                while (true) {
                                    val ch = awaitPointerEvent().changes.first()
                                    ch.consume()
                                    if (!ch.pressed) break
                                }
                                pressed = false
                                stopRecording()
                                return@awaitEachGesture
                            }
                            val released = withTimeoutOrNull(longPressMs) {
                                while (true) {
                                    val ch = awaitPointerEvent().changes.first()
                                    ch.consume()
                                    if (!ch.pressed) return@withTimeoutOrNull true
                                }
                                @Suppress("UNREACHABLE_CODE") false
                            }
                            if (released == true) {
                                pressed = false
                                takePhoto()
                                return@awaitEachGesture
                            }
                            startRecording()
                            var lockedNow = false
                            while (true) {
                                val ch = awaitPointerEvent().changes.first()
                                val d = ch.position - down.position
                                setZoom((-d.y / zoomTravel).coerceIn(0f, 1f))
                                lockProgress = (-d.x / lockTravel).coerceIn(0f, 1f)
                                if (lockProgress >= 1f && !lockedNow) { lockedNow = true; locked = true }
                                ch.consume()
                                if (!ch.pressed) break
                            }
                            pressed = false
                            lockProgress = 0f
                            if (!lockedNow) stopRecording()
                        }
                    },
                )

                Box(
                    Modifier.align(Alignment.CenterEnd).padding(end = 40.dp).size(48.dp).clip(CircleShape)
                        .background(Color.Black.copy(alpha = 0.35f))
                        .graphicsLayer { alpha = if (recording == null) 1f else 0f }
                        .pointerInput(Unit) { detectTapGestures { flip() } },
                    contentAlignment = Alignment.Center,
                ) { DrawableIcon(R.drawable.oi_camera_flip, Modifier.size(24.dp), desc = "Flip camera", tint = Color.White) }
            }
        }
    }
}

@Composable
private fun CloseButton(onClose: () -> Unit, modifier: Modifier) {
    Box(
        modifier.size(44.dp).clip(CircleShape).pointerInput(Unit) { detectTapGestures { onClose() } },
        contentAlignment = Alignment.Center,
    ) { MorphIcon(MorphGlyph.Close, "Close", Modifier.size(24.dp), tint = Color.White, strokeWidth = 2.dp) }
}

/**
 * The shutter, from the `camera_shutter` Lottie: idle 0, press 6, record 18, locked 30, and a
 * release run at 42–54 that only a locked recording uses. Every other change reverses along the
 * frames it came by, so letting go mid-way never jumps. The clip progress arc rides the ring.
 */
@Composable
private fun Shutter(recording: Boolean, locked: Boolean, pressed: Boolean, progress: () -> Float, modifier: Modifier) {
    val composition = rememberLottie(R.raw.camera_shutter)
    val frame = remember { Animatable(0f) }
    val target = when {
        locked -> 30f
        recording -> 18f
        pressed -> 6f
        else -> 0f
    }
    var last by remember { mutableFloatStateOf(0f) }
    LaunchedEffect(target) {
        val wasLocked = last == 30f
        last = target
        if (target == 0f && wasLocked) {
            frame.snapTo(42f)
            frame.animateTo(54f, tween(200, easing = LinearEasing))
            frame.snapTo(0f)
        } else frame.animateTo(target, tween(180, easing = LinearEasing))
    }
    Box(modifier.size(96.dp), contentAlignment = Alignment.Center) {
        LottieFrame(composition, { frame.value }, Modifier.size(80.dp))
        Canvas(Modifier.size(80.dp)) {
            if (!recording) return@Canvas
            val r = 38.dp.toPx()
            drawArc(
                Color.White, -90f, 360f * progress(), useCenter = false,
                topLeft = Offset(center.x - r, center.y - r), size = Size(r * 2, r * 2),
                style = Stroke(4.dp.toPx(), cap = androidx.compose.ui.graphics.StrokeCap.Round),
            )
        }
    }
}

