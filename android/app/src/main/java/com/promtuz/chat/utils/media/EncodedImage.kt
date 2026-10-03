package com.promtuz.chat.utils.media

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import android.graphics.Bitmap
import android.os.SystemClock
import android.view.Window
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Spacer
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onVisibilityChanged
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.window.DialogWindowProvider
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.promtuz.chat.navigation.LocalNavCardExiting
import com.promtuz.chat.navigation.LocalNavForeground
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import org.aomedia.avif.android.AvifDecoder
import timber.log.Timber
import java.io.File
import java.util.WeakHashMap
import kotlin.math.ceil

internal val imageDecodeWork = Semaphore(2)

/** Shared photo/sticker renderer. The original stays encoded; posters never replace its bytes. */
@Composable
fun EncodedImage(
    bytes: ByteArray? = null,
    filePath: String? = null,
    contentDescription: String?,
    modifier: Modifier = Modifier,
    poster: ImageBitmap? = null,
    contentScale: ContentScale = ContentScale.Fit,
    maxEdge: Int = Int.MAX_VALUE,
    animate: Boolean = true,
    onFrameChanged: ((ImageBitmap?) -> Unit)? = null,
) {
    val prepared by produceState<PreparedEncodedImage?>(null, bytes, filePath, maxEdge) {
        value = null
        try {
            val edge = if (maxEdge == Int.MAX_VALUE) 2048 else maxEdge.coerceAtMost(4096)
            if (bytes == null && filePath != null) {
                val file = File(filePath)
                val largeFile = withContext(Dispatchers.IO) { file.length() > MAX_ENCODED_IMAGE_BYTES }
                if (largeFile) {
                    value = withContext(Dispatchers.IO) {
                        imageDecodeWork.withPermit { prepareLargeImageFile(file, edge) }
                    }
                    return@produceState
                }
            }
            val source = withContext(Dispatchers.IO) {
                bytes ?: filePath?.let { path ->
                    val file = File(path)
                    if (file.length() !in 1..MAX_ENCODED_IMAGE_BYTES.toLong()) null else file.readBytes()
                }
            }
            if (source != null) {
                value = withContext(Dispatchers.Default) {
                    imageDecodeWork.withPermit { prepareEncodedImage(source, targetEdge = edge) }
                }
            }
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            Timber.tag("MediaImage").d(error, "Image unavailable")
        }
    }
    EncodedImageContent(prepared, contentDescription, modifier, poster, contentScale, animate, onFrameChanged)
}

@Composable
internal fun EncodedImageContent(
    prepared: PreparedEncodedImage?,
    contentDescription: String?,
    modifier: Modifier = Modifier,
    poster: ImageBitmap? = null,
    contentScale: ContentScale = ContentScale.Fit,
    animate: Boolean = true,
    onFrameChanged: ((ImageBitmap?) -> Unit)? = null,
) {
    var visible by remember { mutableStateOf(false) }
    var frame by remember(prepared, poster) { mutableStateOf(prepared?.poster ?: poster) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val lifecycleState by lifecycle.currentStateFlow.collectAsState()
    val active = visible && LocalNavForeground.current && !LocalNavCardExiting.current &&
        lifecycleState.isAtLeast(Lifecycle.State.STARTED)
    LaunchedEffect(prepared, active, animate, lifecycle) {
        if (active && animate) {
            prepared?.animation?.let { source ->
                lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
                    playAvif(source) { frame = it }
                }
            }
        }
    }
    val image = frame
    SideEffect { onFrameChanged?.invoke(image) }
    ImageWindowColorMode(image, active, prepared?.hdr == true)
    val observedModifier = modifier.onVisibilityChanged(minDurationMs = 80, minFractionVisible = 0.01f) {
        visible = it
    }
    if (image != null) {
        Image(image, contentDescription, observedModifier, contentScale = contentScale)
    } else {
        Spacer(observedModifier)
    }
}

/** A flying image keeps the same shared window color request after its page is disposed. */
@Composable
internal fun ImageWindowColorMode(image: ImageBitmap?, active: Boolean, hdr: Boolean = false) {
    val bitmap = image?.asAndroidBitmap()
    val colorMode = when {
        hdr || bitmap?.let(::bitmapHasHdr) == true -> ActivityInfo.COLOR_MODE_HDR
        bitmap?.colorSpace?.isWideGamut == true -> ActivityInfo.COLOR_MODE_WIDE_COLOR_GAMUT
        else -> ActivityInfo.COLOR_MODE_DEFAULT
    }
    ImageWindowColorMode(active, colorMode)
}

/**
 * The decoder, its buffer and every JNI call have one coroutine owner. Cancellation cannot
 * release a decoder while nextFrame is still running. Published bitmaps are never reused or
 * recycled: Compose's render thread may retain the preceding frame after state changes.
 */
private suspend fun playAvif(source: AvifAnimationSource, onFrame: (ImageBitmap) -> Unit) {
    val (width, height) = scaledDimensions(source.width, source.height, 1024)
    val pixelBytes = if (source.config == Bitmap.Config.RGBA_F16) 8 else 4
    // Include AV1 reference pictures, working planes, two RGB frames and the encoded buffer.
    val estimatedBytes = source.decodedPixelCount * (if (source.depth > 8) 64 else 32) +
        width.toLong() * height * pixelBytes * 2 + source.encoded.capacity()
    try {
        AvifPlaybackBudget.withBudget(estimatedBytes) {
            withContext(Dispatchers.Default) {
                val encoded = source.encoded.duplicate().apply { rewind() }
                val decoder = imageDecodeWork.withPermit { AvifDecoder.create(encoded, 1) }
                    ?: return@withContext
                try {
                    if (decoder.frameCount !in 2..600) return@withContext
                    val durations = decoder.frameDurations
                    var index = 0
                    var completedRepeats = 0
                    var nextPresentation = SystemClock.elapsedRealtimeNanos()
                    while (true) {
                        currentCoroutineContext().ensureActive()
                        val bitmap = imageDecodeWork.withPermit {
                            val target = Bitmap.createBitmap(width, height, source.config,
                                source.alpha, source.colorSpace)
                            val result = if (index == 0) decoder.nthFrame(0, target) else decoder.nextFrame(target)
                            if (result != 0) {
                                target.recycle()
                                error("AVIF frame decode failed: ${AvifDecoder.resultToString(result)}")
                            }
                            orientAvifBitmap(target, source.rotationQuarterTurns, source.mirrorAxis)
                        }
                        currentCoroutineContext().ensureActive()
                        val waitNanos = nextPresentation - SystemClock.elapsedRealtimeNanos()
                        if (waitNanos > 0) delay(ceil(waitNanos / 1_000_000.0).toLong())
                        withContext(Dispatchers.Main.immediate) {
                            withFrameNanos { onFrame(bitmap.asImageBitmap()) }
                        }
                        val duration = durations.getOrNull(index)?.takeIf { it.isFinite() && it > 0 } ?: 0.1
                        // A malformed duration cannot create a busy loop. Valid fast animations
                        // remain synchronized to display frames without queuing decoded frames.
                        val durationNanos = (duration * 1_000_000_000).toLong().coerceIn(8_000_000L, 300_000_000_000L)
                        nextPresentation = maxOf(nextPresentation, SystemClock.elapsedRealtimeNanos()) + durationNanos
                        index++
                        if (index == decoder.frameCount) {
                            if (decoder.repetitionCount != -1 && completedRepeats >= decoder.repetitionCount) break
                            completedRepeats++
                            index = 0
                        }
                    }
                } finally {
                    decoder.release()
                }
            }
        }
    } catch (error: CancellationException) {
        throw error
    } catch (error: LinkageError) {
        Timber.tag("MediaImage").w(error, "AVIF playback unavailable")
    } catch (error: Exception) {
        Timber.tag("MediaImage").d(error, "AVIF playback stopped")
    }
}

/** Bound native memory as well as worker count; a large photo cannot displace dozens of decoders. */
private object AvifPlaybackBudget {
    private const val UNIT = 1024 * 1024L
    private const val CAPACITY = 128
    private val memory = Semaphore(CAPACITY)
    private val reservations = Mutex()
    private val sessions = Semaphore(8)

    suspend fun withBudget(bytes: Long, block: suspend () -> Unit) {
        val units = ((bytes + UNIT - 1) / UNIT).toInt().coerceAtLeast(1)
        if (units > CAPACITY) return // Keep the bounded platform poster for oversized sequences.
        sessions.withPermit {
            var acquired = 0
            try {
                reservations.withLock {
                    repeat(units) {
                        memory.acquire()
                        acquired++
                    }
                }
                block()
            } finally {
                repeat(acquired) { memory.release() }
            }
        }
    }
}

@Composable
private fun ImageWindowColorMode(active: Boolean, colorMode: Int) {
    val view = LocalView.current
    val window = (view.parent as? DialogWindowProvider)?.window ?: view.context.activity()?.window
    DisposableEffect(window, active, colorMode) {
        val token = Any()
        if (window != null && active && colorMode != ActivityInfo.COLOR_MODE_DEFAULT) {
            ImageWindowColorModes.add(window, token, colorMode)
        }
        onDispose { if (window != null) ImageWindowColorModes.remove(window, token) }
    }
}

/** Main-thread requests are shared by all images in a window and restore its previous mode. */
private object ImageWindowColorModes {
    private class Requests(val original: Int, val modes: MutableMap<Any, Int> = mutableMapOf())
    private val windows = WeakHashMap<Window, Requests>()

    fun add(window: Window, token: Any, mode: Int) {
        val requests = windows.getOrPut(window) { Requests(window.colorMode) }
        requests.modes[token] = mode
        window.colorMode = if (requests.modes.values.any { it == ActivityInfo.COLOR_MODE_HDR }) {
            ActivityInfo.COLOR_MODE_HDR
        } else ActivityInfo.COLOR_MODE_WIDE_COLOR_GAMUT
    }

    fun remove(window: Window, token: Any) {
        val requests = windows[window] ?: return
        if (requests.modes.remove(token) == null) return
        if (requests.modes.isEmpty()) {
            window.colorMode = requests.original
            windows.remove(window)
        } else {
            window.colorMode = if (requests.modes.values.any { it == ActivityInfo.COLOR_MODE_HDR }) {
                ActivityInfo.COLOR_MODE_HDR
            } else ActivityInfo.COLOR_MODE_WIDE_COLOR_GAMUT
        }
    }
}

private tailrec fun Context.activity(): Activity? = when (this) {
    is Activity -> this
    is ContextWrapper -> if (baseContext !== this) baseContext.activity() else null
    else -> null
}
