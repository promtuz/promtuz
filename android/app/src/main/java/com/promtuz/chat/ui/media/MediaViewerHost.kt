package com.promtuz.chat.ui.media

import android.widget.Toast
import androidx.activity.BackEventCompat
import androidx.activity.compose.LocalActivity
import androidx.activity.compose.PredictiveBackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculateCentroid
import androidx.compose.foundation.gestures.calculatePan
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.mutableIntStateOf
import com.promtuz.chat.ui.stage.ChatMotion
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.RoundRect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.geometry.center
import androidx.compose.ui.geometry.lerp
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.util.lerp
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.compose.ui.graphics.TransformOrigin
import com.promtuz.chat.ui.util.deviceCornerRadius
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.utils.media.saveToGallery
import com.promtuz.chat.utils.media.saveFileToGallery
import com.promtuz.chat.utils.media.saveEncodedImageToGallery
import com.promtuz.chat.utils.media.shareFile
import com.promtuz.chat.utils.media.sharePicture
import com.promtuz.chat.utils.media.shareEncodedImage
import com.promtuz.chat.utils.media.EncodedImage
import com.promtuz.chat.utils.media.ImageWindowColorMode
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import androidx.compose.runtime.snapshotFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.delay
import kotlin.math.abs
import kotlin.math.roundToInt

private const val OPEN_MS = 300
private const val CLOSE_MS = 260
private const val FLIGHT_HOLD_MS = 200L
private val flightEase = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)
private val DISMISS_THRESHOLD = 96.dp
private const val BACK_SCALE = 0.90f
private val BACK_CORNER = 24.dp

private enum class Phase { Opening, Open, Closing }

/** A copy of the picture flying between rects; the entry point itself never moves. */
private class Flight(
    val key: String,
    val image: ImageBitmap?,
    val from: Rect,
    val to: Rect,
    val fromRadius: Float,
    val toRadius: Float,
    val fade: Boolean,
    /** The visible band at either end; null is the whole screen. */
    val fromClip: Rect? = null,
    val toClip: Rect? = null,
    val fromAlpha: Float = 1f,
)

/** Mount once above navigation. */
@Composable
fun MediaViewerHost() {
    val session = MediaViewer.session ?: return
    key(session) { Viewer(session) }
}

@Composable
private fun Viewer(session: MediaSession) {
    val app = org.koin.compose.koinInject<com.promtuz.chat.presentation.viewmodel.AppVM>()
    val density = LocalDensity.current
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val view = LocalView.current
    val activity = LocalActivity.current

    val pager = rememberPagerState(session.startIndex) { session.items.size }
    val zooms = remember { HashMap<String, ZoomState>() }
    val frames = remember { HashMap<String, MediaFrame>() }
    fun zoomOf(key: String) = zooms.getOrPut(key) { ZoomState() }

    var phase by remember { mutableStateOf(Phase.Opening) }
    var flight by remember { mutableStateOf<Flight?>(null) }
    var progress by remember { mutableStateOf(Animatable(0f)) }
    var closeAction by remember { mutableStateOf<(() -> Unit)?>(null) }
    val dismissY = remember { Animatable(0f) }
    val backScale = remember { Animatable(0f) }
    var chrome by remember { mutableStateOf(true) }
    var viewport by remember { mutableStateOf(Size.Zero) }
    val hostCoords = remember { arrayOfNulls<LayoutCoordinates>(1) }

    val current = session.items.getOrNull(pager.currentPage)
    var backActive by remember { mutableStateOf(false) }
    var backEdgeRight by remember { mutableStateOf(false) }
    var backTouchY by remember { mutableStateOf(0f) }
    val deviceRadius = deviceCornerRadius()
    val keyboard = LocalSoftwareKeyboardController.current
    LaunchedEffect(Unit) { keyboard?.hide() }

    // The overlay is black under any theme, so the status bar icons must go light for the duration.
    val insets = remember(activity, view) { activity?.window?.let { WindowCompat.getInsetsController(it, view) } }
    DisposableEffect(insets) {
        val controller = insets ?: return@DisposableEffect onDispose {}
        val wasLight = controller.isAppearanceLightStatusBars
        controller.isAppearanceLightStatusBars = false
        controller.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        onDispose {
            controller.isAppearanceLightStatusBars = wasLight
            controller.show(WindowInsetsCompat.Type.systemBars())
        }
    }

    fun originRect(item: MediaItem) = MediaViewer.originRectIn(item.key, hostCoords[0], density)

    fun currentRect(): Rect {
        val f = flight
        if (phase != Phase.Open && f != null) return lerp(f.from, f.to, progress.value)
        val item = session.items.getOrNull(pager.currentPage) ?: return Rect.Zero
        val z = zoomOf(item.key)
        var r = if (z.viewport == Size.Zero) fitInto(item.width, item.height, viewport) else z.displayed()
        r = r.translate(0f, dismissY.value)
        // The back-swipe card scales about the finger's side, the same as a screen does.
        val s = lerp(1f, BACK_SCALE, backScale.value)
        if (s != 1f) {
            val c = Offset(viewport.width * (if (backEdgeRight) 0.12f else 0.88f), viewport.height * (backTouchY / viewport.height).coerceIn(0f, 1f))
            r = Rect(c + (r.topLeft - c) * s, r.size * s)
        }
        return r
    }

    fun close(afterClose: () -> Unit = {}) {
        val item = session.items.getOrNull(pager.currentPage) ?: run { MediaViewer.close(); afterClose(); return }
        if (phase == Phase.Closing) return
        val from = currentRect()
        val priorFlight = flight?.takeIf { it.key == item.key }
        val interruptedOpening = phase == Phase.Opening
        val openingProgress = progress.value
        val target = originRect(item)
        // No origin on screen: the picture lives above or below the chat's viewport, older
        // items above, so it leaves that way rather than shrinking into the middle.
        val above = pager.currentPage < session.startIndex
        val gone = Rect(
            Offset(viewport.center.x - from.width * 0.3f, if (above) -from.height * 0.6f else viewport.height),
            from.size * 0.6f,
        )
        flight = Flight(
            key = item.key,
            image = priorFlight?.image ?: frames[item.key]?.image ?: MediaViewer.originImage(item.key) ?: item.thumb,
            from = from, to = target?.first ?: gone,
            fromRadius = if (interruptedOpening && priorFlight != null) {
                lerp(priorFlight.fromRadius, priorFlight.toRadius, openingProgress)
            } else 0f,
            toRadius = target?.second ?: 0f,
            fade = target == null,
            fromClip = if (interruptedOpening && priorFlight != null) {
                val full = Rect(Offset.Zero, viewport)
                lerp(priorFlight.fromClip ?: full, priorFlight.toClip ?: full, openingProgress)
            } else null,
            toClip = if (target != null) MediaViewer.originClipIn(item.key, hostCoords[0]) else null,
            fromAlpha = if (interruptedOpening && priorFlight?.fade == true) openingProgress else 1f,
        )
        // A fresh progress object starts the return at this exact rect, even mid-opening.
        progress = Animatable(0f)
        closeAction = afterClose
        phase = Phase.Closing
    }

    LaunchedEffect(phase) {
        when (phase) {
            Phase.Opening -> {
                val vp = snapshotFlow { viewport }.first { it != Size.Zero }
                val item = session.items[session.startIndex]
                val image = MediaViewer.originImage(item.key) ?: item.thumb
                frames.getOrPut(item.key) { MediaFrame() }.image = image
                val to = fitInto(image?.width ?: item.width, image?.height ?: item.height, vp)
                val origin = originRect(item)
                flight = Flight(
                    key = item.key, image = image,
                    from = origin?.first ?: Rect(to.center - Offset(to.width * 0.25f, to.height * 0.25f), to.size * 0.5f),
                    to = to,
                    fromRadius = origin?.second ?: 0f, toRadius = 0f,
                    fade = origin == null,
                    fromClip = if (origin != null) MediaViewer.originClipIn(item.key, hostCoords[0]) else null,
                )
                val opening = progress
                opening.animateTo(1f, tween(OPEN_MS, easing = flightEase))
                if (phase == Phase.Opening && progress === opening) phase = Phase.Open
            }
            Phase.Open -> {
                // Keep the landed copy while the page (or a video's surface) gets its first frame.
                val landed = flight
                delay(FLIGHT_HOLD_MS)
                if (phase == Phase.Open && flight === landed) flight = null
            }
            Phase.Closing -> {
                progress.animateTo(1f, tween(CLOSE_MS, easing = flightEase))
                if (MediaViewer.session === session) {
                    MediaViewer.close()
                    closeAction?.invoke()
                }
            }
        }
    }

    ImageWindowColorMode(flight?.image, active = flight != null)

    PredictiveBackHandler { events: Flow<BackEventCompat> ->
        backActive = true
        try {
            events.collect {
                backEdgeRight = it.swipeEdge == BackEventCompat.EDGE_RIGHT
                backTouchY = it.touchY
                backScale.snapTo(it.progress)
            }
            close()
            backScale.snapTo(0f)
            backActive = false
        } catch (c: Throwable) {
            backActive = false
            scope.launch { backScale.animateTo(0f, spring()) }
            throw c
        }
    }

    val dismissThresholdPx = with(density) { DISMISS_THRESHOLD.toPx() }
    val chromeShown = chrome && phase != Phase.Closing && dismissY.value == 0f && !backActive
    val chromeAlpha by animateFloatAsState(if (chromeShown) 1f else 0f, tween(240, easing = ChatMotion.Easing), label = "viewer chrome")
    var bottomChromePx by remember { mutableIntStateOf(0) }
    LaunchedEffect(chrome, phase) {
        val c = insets ?: return@LaunchedEffect
        if (phase != Phase.Open || chrome) c.show(WindowInsetsCompat.Type.systemBars())
        else c.hide(WindowInsetsCompat.Type.systemBars())
    }

    Box(
        Modifier
            .fillMaxSize()
            .onSizeChanged { viewport = Size(it.width.toFloat(), it.height.toFloat()) }
            .onGloballyPositioned { hostCoords[0] = it }
            .drawBehind { drawRect(Color.Black, alpha = 0.2f * backScale.value) },
    ) {
        // The black ground and the pages scale together as one card on a back swipe.
        Box(
            Modifier
                .fillMaxSize()
                .graphicsLayer {
                    val p = backScale.value
                    val s = lerp(1f, BACK_SCALE, p)
                    scaleX = s
                    scaleY = s
                    transformOrigin = TransformOrigin(
                        if (backEdgeRight) 0.12f else 0.88f,
                        (backTouchY / size.height).coerceIn(0f, 1f),
                    )
                    clip = true
                    shape = RoundedCornerShape(lerp(deviceRadius.toPx(), BACK_CORNER.toPx(), p))
                }
                .drawBehind {
                    val p = progress.value
                    val base = when (phase) {
                        Phase.Opening -> p
                        Phase.Open -> 1f
                        Phase.Closing -> 1f - p
                    }
                    val drag = 1f - 0.85f * (abs(dismissY.value) / (size.height * 0.5f)).coerceIn(0f, 1f)
                    drawRect(Color.Black, alpha = base * drag)
                },
        ) {
        if (phase == Phase.Open) {
            HorizontalPager(
                pager,
                Modifier.fillMaxSize(),
                beyondViewportPageCount = 1,
                pageSpacing = 16.dp,
                userScrollEnabled = current?.let { !zoomOf(it.key).zoomed } ?: true,
            ) { index ->
                val item = session.items[index]
                val frame = remember(item.key) {
                    frames.getOrPut(item.key) {
                        MediaFrame().apply { image = MediaViewer.originImage(item.key) ?: item.thumb }
                    }
                }
                DisposableEffect(item.key, frame) {
                    onDispose { if (frames[item.key] === frame) frames.remove(item.key) }
                }
                Page(
                    item = item,
                    frame = frame,
                    zoom = zoomOf(item.key),
                    dismissY = dismissY,
                    dismissThreshold = dismissThresholdPx,
                    onTap = { chrome = !chrome },
                    onDismiss = { close() },
                    active = pager.settledPage == index,
                    chrome = chrome,
                    bottomChrome = { bottomChromePx },
                )
            }
        }
        }

        // The flying copy, above the pager and below the chrome.
        Box(Modifier.fillMaxSize().drawBehind {
            val f = flight ?: return@drawBehind
            val p = progress.value
            val rect = lerp(f.from, f.to, p)
            val radius = lerp(f.fromRadius, f.toRadius, p)
            val alpha = if (phase == Phase.Closing) f.fromAlpha * (if (f.fade) 1f - p else 1f)
                else if (f.fade) p else 1f
            val img = f.image
            val full = Rect(Offset.Zero, size)
            val band = lerp(f.fromClip ?: full, f.toClip ?: full, p)
            clipRect(band.left, band.top, band.right, band.bottom) {
                clipPath(Path().apply { addRoundRect(RoundRect(rect, CornerRadius(radius))) }) {
                    if (img != null) drawCover(img, rect, alpha)
                    else drawRect(Color.White.copy(alpha = 0.1f * alpha), rect.topLeft, rect.size)
                }
            }
        })

        Chrome(
            item = current,
            items = session.items,
            index = pager.currentPage,
            onSelect = { i -> scope.launch { pager.animateScrollToPage(i) } },
            alpha = if (phase == Phase.Opening) chromeAlpha * progress.value else chromeAlpha,
            onBottomHeight = { bottomChromePx = it },
            onBack = { close() },
            onShowInChat = { item -> item.message?.let { message -> close { app.showMessage(message) } } },
            onSave = { item ->
                scope.launch {
                    val file = item.filePath
                    val bytes = item.encoded
                    val ok = try {
                        when {
                            file != null -> saveFileToGallery(context, file, item.mime, item.shareName)
                            bytes != null -> saveEncodedImageToGallery(context, bytes, item.mime, item.shareName)
                            else -> (item.load() ?: item.thumb)?.let { saveToGallery(context, it, item.shareName) } == true
                        }
                    } catch (e: CancellationException) { throw e }
                    catch (_: Exception) { false }
                    Toast.makeText(context, if (ok) "Saved to gallery" else "Couldn’t save", Toast.LENGTH_SHORT).show()
                }
            },
            onShare = { item ->
                scope.launch {
                    val file = item.filePath
                    val bytes = item.encoded
                    try {
                        when {
                            file != null -> shareFile(context, java.io.File(file), item.mime)
                            bytes != null -> shareEncodedImage(context, bytes, item.mime, item.shareName)
                            else -> (item.load() ?: item.thumb)?.let { sharePicture(context, it, item.shareName) }
                        }
                    } catch (e: CancellationException) { throw e }
                    catch (_: Exception) { Toast.makeText(context, "Couldn’t share", Toast.LENGTH_SHORT).show() }
                }
            },
        )
    }
}

/** Draws [image] scaled to cover [rect], centred, the way the entry points crop it. */
private fun DrawScope.drawCover(image: ImageBitmap, rect: Rect, alpha: Float) {
    val scale = maxOf(rect.width / image.width, rect.height / image.height)
    val w = (image.width * scale).roundToInt()
    val h = (image.height * scale).roundToInt()
    drawImage(
        image,
        dstOffset = IntOffset(
            (rect.left + (rect.width - w) / 2f).roundToInt(),
            (rect.top + (rect.height - h) / 2f).roundToInt(),
        ),
        dstSize = IntSize(w, h),
        alpha = alpha,
    )
}

@Composable
private fun Page(
    item: MediaItem,
    frame: MediaFrame,
    zoom: ZoomState,
    dismissY: Animatable<Float, *>,
    dismissThreshold: Float,
    onTap: () -> Unit,
    onDismiss: () -> Unit,
    active: Boolean,
    chrome: Boolean,
    bottomChrome: () -> Int,
) {
    val scope = rememberCoroutineScope()
    val touchSlop = LocalViewConfiguration.current.touchSlop
    val poster = remember(item.key) { frame.image ?: item.thumb }
    val full by produceState(poster, item) {
        value = if (item.encoded == null && item.filePath == null) item.load() ?: poster else poster
    }
    val imageSize = frame.size.takeUnless { it == IntSize.Zero } ?: IntSize(item.width, item.height)
    SideEffect { zoom.fitted = fitInto(imageSize.width, imageSize.height, zoom.viewport).size }

    Box(
        Modifier
            .fillMaxSize()
            .onSizeChanged {
                zoom.viewport = Size(it.width.toFloat(), it.height.toFloat())
                zoom.fitted = fitInto(imageSize.width, imageSize.height, zoom.viewport).size
            }
            .pointerInput(zoom) {
                detectTapGestures(
                    onTap = { onTap() },
                    onDoubleTap = { tap ->
                        val (s, o) = zoom.doubleTapTarget(tap)
                        scope.launch { zoom.animateTo(s, o, spring(stiffness = 600f), spring(stiffness = 600f)) }
                    },
                )
            }
            .pointerInput(zoom) {
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    var decided: Gesture? = null
                    var travelled = Offset.Zero
                    while (true) {
                        val event = awaitPointerEvent(PointerEventPass.Main)
                        val pressed = event.changes.filter { it.pressed }
                        if (pressed.isEmpty()) break
                        if (pressed.size >= 2) {
                            decided = Gesture.Pinch
                            zoom.zoomBy(event.calculateZoom(), event.calculateCentroid())
                            zoom.panBy(event.calculatePan())
                            event.changes.forEach { it.consume() }
                            continue
                        }
                        val change = pressed.first()
                        val delta = change.positionChange()
                        if (decided == null && change.isConsumed) decided = Gesture.Pager
                        if (decided == null) {
                            travelled = change.position - down.position
                            if (travelled.getDistance() < touchSlop) continue
                            decided = when {
                                zoom.zoomed -> Gesture.Pan
                                abs(travelled.y) > abs(travelled.x) -> Gesture.Dismiss
                                else -> Gesture.Pager
                            }
                        }
                        when (decided) {
                            Gesture.Pan -> {
                                zoom.panBy(delta)
                                change.consume()
                            }
                            Gesture.Dismiss -> {
                                scope.launch { dismissY.snapTo(dismissY.value + delta.y) }
                                change.consume()
                            }
                            Gesture.Pinch -> {
                                zoom.panBy(delta)
                                change.consume()
                            }
                            else -> Unit
                        }
                    }

                    when (decided) {
                        Gesture.Dismiss ->
                            if (abs(dismissY.value) > dismissThreshold) onDismiss()
                            else scope.launch { dismissY.animateTo(0f, spring(stiffness = 500f)) }
                        Gesture.Pinch, Gesture.Pan -> {
                            val (s, o) = zoom.settleTarget()
                            if (s != zoom.scale || o != zoom.offset) scope.launch {
                                zoom.animateTo(s, o, spring(stiffness = 600f), spring(stiffness = 600f))
                            }
                        }
                        else -> Unit
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        val bitmap = full
        val fitted = fitInto(imageSize.width, imageSize.height, zoom.viewport)
        val video = item.videoPath?.let { rememberVideoPlayer(it, active, adopt = InlinePlayback.adopt(item.key)) }
        Box(
            Modifier
                .size(with(LocalDensity.current) { fitted.width.toDp() }, with(LocalDensity.current) { fitted.height.toDp() })
                .graphicsLayer {
                    scaleX = zoom.scale
                    scaleY = zoom.scale
                    translationX = zoom.offset.x
                    translationY = zoom.offset.y + dismissY.value
                },
        ) {
            when {
                video != null -> VideoSurface(video, item, Modifier.fillMaxSize())
                else -> EncodedImage(
                    bytes = item.encoded, filePath = item.filePath, poster = bitmap, contentDescription = null,
                    modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Fit,
                    maxEdge = 4096, animate = active,
                    onFrameChanged = { frame.image = it },
                )
            }
        }
        if (video != null) VideoControls(
            video, chrome,
            bottomInset = with(LocalDensity.current) {
                WindowInsets.navigationBars.getBottom(this).toDp() + bottomChrome().toDp()
            } + 20.dp,
        )
    }
}

private enum class Gesture { Pinch, Pan, Dismiss, Pager }

@Composable
private fun Chrome(
    item: MediaItem?,
    items: List<MediaItem>,
    index: Int,
    onSelect: (Int) -> Unit,
    alpha: Float,
    onBottomHeight: (Int) -> Unit,
    onBack: () -> Unit,
    onSave: (MediaItem) -> Unit,
    onShare: (MediaItem) -> Unit,
    onShowInChat: (MediaItem) -> Unit,
) {
    if (item == null || alpha == 0f) return
    var showInfo by remember(item.key) { mutableStateOf(false) }
    if (showInfo) MediaInfoDialog(item) { showInfo = false }
    val white = Color.White
    val slide = with(LocalDensity.current) { 40.dp.toPx() }
    Box(Modifier.fillMaxSize()) {
        Row(
            Modifier
                .fillMaxWidth()
                .graphicsLayer { this.alpha = alpha; translationY = -(1f - alpha) * slide }
                .background(Brush.verticalGradient(listOf(Color.Black.copy(alpha = 0.7f), Color.Transparent)))
                .statusBarsPadding()
                .padding(bottom = 36.dp)
                .height(56.dp)
                .padding(start = 6.dp, end = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier.size(44.dp).clip(CircleShape).pointerInput(Unit) { detectTapGestures { onBack() } },
                contentAlignment = Alignment.Center,
            ) { MorphIcon(MorphGlyph.Back, "Back", Modifier.size(24.dp), tint = white, strokeWidth = 2.dp) }
            Column(Modifier.weight(1f).padding(start = 6.dp)) {
                Text(item.title, style = MaterialTheme.typography.titleMedium, color = white, maxLines = 1,
                    overflow = TextOverflow.Ellipsis)
                if (item.subtitle.isNotEmpty()) Text(
                    item.subtitle, style = MaterialTheme.typography.labelMedium, color = white.copy(alpha = 0.7f),
                    maxLines = 1,
                )
            }
            AppDropMenu(
                iconSize = 20.dp,
                anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "More", tint = white) },
                groups = buildList {
                    add(listOf(
                        MenuAction("Save", R.drawable.oi_image_save) { onSave(item) },
                        MenuAction("Share", R.drawable.oi_export) { onShare(item) },
                        MenuAction("Info", R.drawable.oi_info) { showInfo = true },
                    ))
                    if (item.message != null) add(listOf(
                        MenuAction("Show in chat", R.drawable.oi_message_jump) { onShowInChat(item) },
                    ))
                    addAll(item.actions)
                },
            )
        }
        val album = item.group?.let { g -> items.withIndex().filter { it.value.group == g } }?.takeIf { it.size > 1 }
        Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .graphicsLayer { this.alpha = alpha; translationY = (1f - alpha) * slide }
                .background(Brush.verticalGradient(listOf(Color.Transparent, Color.Black.copy(alpha = 0.8f))))
                .padding(top = 56.dp)
                .navigationBarsPadding(),
        ) {
          Column(Modifier.fillMaxWidth().onSizeChanged { onBottomHeight(it.height) }) {
            if (item.caption.isNotEmpty()) Text(
                item.caption, style = MaterialTheme.typography.bodyMedium, color = white, maxLines = 4,
                overflow = TextOverflow.Ellipsis, modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp),
            )
            if (album != null) Filmstrip(
                album.map { it.value }, album.indexOfFirst { it.index == index }.coerceAtLeast(0),
                onSelect = { onSelect(album[it].index) },
            )
          }
        }
    }
}

@Composable
private fun Filmstrip(items: List<MediaItem>, index: Int, onSelect: (Int) -> Unit) {
    val state = rememberLazyListState()
    val density = LocalDensity.current
    LaunchedEffect(index) {
        val viewport = state.layoutInfo.viewportSize.width
        val slot = with(density) { (FilmCurrent + FilmGap).roundToPx() }
        state.animateScrollToItem(index, -(viewport / 2 - slot / 2))
    }
    LazyRow(
        state = state,
        modifier = Modifier.fillMaxWidth().height(FilmCurrent + 16.dp),
        contentPadding = PaddingValues(horizontal = 16.dp, vertical = 8.dp),
        horizontalArrangement = Arrangement.spacedBy(FilmGap),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        itemsIndexed(items, key = { _, it -> it.key }) { i, item ->
            val ratio = (if (item.width > 0 && item.height > 0) item.width.toFloat() / item.height else 1f).coerceIn(0.7f, 1.5f)
            val height by animateDpAsState(if (i == index) FilmCurrent else FilmHeight, tween(160), label = "film")
            Box(
                Modifier
                    .height(height)
                    .width(height * ratio)
                    .clip(RoundedCornerShape(6.dp))
                    .background(Color.White.copy(alpha = 0.15f))
                    .pointerInput(i) { detectTapGestures { onSelect(i) } },
            ) {
                item.thumb?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) }
            }
        }
    }
}

private val FilmHeight = 48.dp
private val FilmCurrent = 64.dp
private val FilmGap = 4.dp
