package com.promtuz.chat.navigation

import androidx.activity.BackEventCompat
import androidx.activity.compose.PredictiveBackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.AnimationVector1D
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.onPlaced
import androidx.compose.ui.platform.LocalWindowInfo
import androidx.compose.ui.unit.dp
import androidx.compose.ui.util.lerp
import androidx.lifecycle.viewmodel.navigation3.rememberViewModelStoreNavEntryDecorator
import androidx.navigation3.runtime.NavBackStack
import androidx.navigation3.runtime.NavEntry
import androidx.navigation3.runtime.NavKey
import androidx.navigation3.runtime.rememberDecoratedNavEntries
import androidx.navigation3.runtime.rememberSaveableStateHolderNavEntryDecorator
import com.promtuz.chat.ui.util.deviceCornerRadius
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/** Only the current entry owns read/notification visibility, never a revealed back-preview. */
val LocalNavForeground = compositionLocalOf { true }

/** True while a card animates off; freezeOnExit freezes live blur before the scale hits it. */
val LocalNavCardExiting = compositionLocalOf { false }

private const val FWD_DUR = 260
private const val COMMIT_DUR = 340
private const val CANCEL_DUR = 260
private const val SCALE_TO = 0.90f
private val PBG_CORNER = 24.dp // back-swipe rounds a card up to at least this (flat screens included)
private const val SCRIM_MAX = 0.2f // dim over the revealed screen while back-swiping; lifts on commit
// Caps one frame's share of the push (about 2 frames at 60 fps) so a hitch can't jump the slide.
private const val FRAME_CAP_NANOS = 33_000_000L
private val fwdEase = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)
private val commitEase = CubicBezierEasing(0.3f, 0f, 0.1f, 1f) // EASE_OUT_QUINT-ish fling

/** A popped card sliding off as a detached ghost, no longer on the stack. */
private class ExitingCard(
    val entry: NavEntry<NavKey>,
    val scale: Float,
    val edgeRight: Boolean,
    val touchYFrac: Float,
    val cornerProgress: Float,
    val commit: Animatable<Float, AnimationVector1D>,
)

/**
 * Single-stack navigation on nav3's entry primitives. A released back-swipe pops at once, and the
 * card slides off as a detached [ExitingCard] so the next swipe targets the screen beneath.
 */
@Composable
fun NavStage(
    backStack: NavBackStack<NavKey>,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
    entryProvider: (NavKey) -> NavEntry<NavKey>,
) {
    val entries = rememberDecoratedNavEntries(
        backStack,
        listOf(
            rememberSaveableStateHolderNavEntryDecorator(),
            rememberViewModelStoreNavEntryDecorator(),
        ),
        entryProvider,
    )
    val top = entries.last()
    val below = entries.getOrNull(entries.lastIndex - 1)

    val deviceRadius = deviceCornerRadius()
    val pbgCorner = maxOf(deviceRadius, PBG_CORNER)
    val restShape = RoundedCornerShape(deviceRadius)
    val size = LocalWindowInfo.current.containerSize
    val widthPx = size.width.toFloat().coerceAtLeast(1f)
    val heightPx = size.height.toFloat().coerceAtLeast(1f)
    val scope = rememberCoroutineScope()

    // `forward` is derived in composition, not an effect, so the new screen starts offscreen.
    val topKey = top.contentKey
    var shownKey by remember { mutableStateOf(topKey) }
    var shownSize by remember { mutableIntStateOf(backStack.size) }
    var shownEntry by remember { mutableStateOf(top) }
    var replacementBackground by remember { mutableStateOf<NavEntry<NavKey>?>(null) }
    val forward = topKey != shownKey && backStack.size >= shownSize && backStack.size > 1
    val replacing = forward && backStack.size == shownSize
    val enter = remember { Animatable(1f) } // 0 = new fully offscreen right, 1 = settled
    var pushing by remember { mutableStateOf(false) }
    val exiting = remember { mutableStateListOf<ExitingCard>() }
    val placed = remember(topKey) { mutableStateOf(false) }
    LaunchedEffect(topKey) {
        val isForward = topKey != shownKey && backStack.size >= shownSize && backStack.size > 1
        replacementBackground = shownEntry.takeIf { isForward && backStack.size == shownSize }
        shownEntry = top
        shownKey = topKey
        shownSize = backStack.size
        if (isForward) {
            pushing = true
            try {
                // Reopening a chat whose ghost is still sliding off starts from the ghost's position.
                val startEnter = exiting.firstOrNull { it.entry.contentKey == topKey }
                    ?.let { 1f - it.commit.value } ?: 0f
                enter.snapTo(startEnter)
                // Park offscreen until the incoming screen lays out, so its first frame doesn't eat the slide.
                withTimeoutOrNull(250) { snapshotFlow { placed.value }.first { it } }
                val durNanos = FWD_DUR * 1_000_000L
                var last = withFrameNanos { it }
                var elapsed = 0L
                while (elapsed < durNanos) {
                    val now = withFrameNanos { it }
                    elapsed += (now - last).coerceAtMost(FRAME_CAP_NANOS)
                    last = now
                    val t = fwdEase.transform((elapsed.toFloat() / durNanos).coerceIn(0f, 1f))
                    enter.snapTo(startEnter + (1f - startEnter) * t)
                }
                enter.snapTo(1f)
            } finally {
                pushing = false
                replacementBackground = null
            }
        }
    }
    // Read enter.value only inside graphicsLayer lambdas, so the push animates without recomposing.
    val showPush = forward || pushing

    var backActive by remember(topKey) { mutableStateOf(false) }
    var touchY by remember { mutableFloatStateOf(heightPx / 2f) }
    var fromRight by remember { mutableStateOf(false) }
    val progress = remember(topKey) { Animatable(0f) }
    var backGeneration by remember(topKey) { mutableIntStateOf(0) }
    var backRecovery by remember(topKey) { mutableStateOf<Job?>(null) }

    PredictiveBackHandler(enabled = entries.size > 1) { events: Flow<BackEventCompat> ->
        val generation = ++backGeneration
        backRecovery?.cancel()
        backActive = true
        try {
            // Stop an interrupted return animation at its current position.
            progress.stop()
            val startProgress = progress.value
            events.collect { e ->
                touchY = e.touchY
                fromRight = e.swipeEdge == BackEventCompat.EDGE_RIGHT
                progress.snapTo(lerp(startProgress, 1f, e.progress))
            }
            // Released: pop now and slide the ghost off in `scope`, which outlives this gesture's coroutine.
            val leaving = ExitingCard(
                entry = top,
                scale = lerp(1f, SCALE_TO, progress.value),
                edgeRight = fromRight,
                touchYFrac = (touchY / heightPx).coerceIn(0f, 1f),
                cornerProgress = progress.value,
                commit = Animatable(0f),
            )
            exiting.add(leaving)
            scope.launch {
                try {
                    leaving.commit.animateTo(1f, tween(COMMIT_DUR, easing = commitEase))
                } finally {
                    exiting.remove(leaving)
                }
            }
            onBack()
            backActive = false
        } finally {
            if (backActive && generation == backGeneration) {
                // Android cancels the gesture coroutine itself, so the return runs in `scope`;
                // otherwise the chat stays frozen and never resumes its read lifecycle.
                backRecovery = scope.launch {
                    try {
                        progress.animateTo(0f, tween(CANCEL_DUR, easing = fwdEase))
                    } finally {
                        if (generation == backGeneration) backActive = false
                    }
                }
            }
        }
    }

    // A reopened card's first frame starts at its ghost's offset, before the push effect runs.
    val firstFrameEnter = if (forward) {
        exiting.firstOrNull { it.entry.contentKey == topKey }?.let { 1f - it.commit.value } ?: 0f
    } else 0f

    val frontMod = when {
        backActive -> Modifier.graphicsLayer {
            val s = lerp(1f, SCALE_TO, progress.value)
            scaleX = s
            scaleY = s
            transformOrigin = TransformOrigin(
                if (fromRight) 0.12f else 0.88f,
                (touchY / heightPx).coerceIn(0f, 1f),
            )
            clip = true
            shape = RoundedCornerShape(lerp(deviceRadius.toPx(), pbgCorner.toPx(), progress.value))
        }
        showPush -> Modifier
            .graphicsLayer { translationX = (1f - (if (forward) firstFrameEnter else enter.value)) * widthPx }
            .clip(restShape)
        else -> Modifier.clip(restShape)
    }

    Box(modifier.fillMaxSize()) {
        // One keyed loop, bottom to top, so a screen changing role is moved by Compose rather
        // than disposed, which would reset its state.
        val layers = buildList {
            val seen = HashSet<Any?>()
            val background = if (showPush) {
                if (replacing) shownEntry else replacementBackground ?: below
            } else below
            if ((backActive || showPush) && background != null) {
                add(Triple(background, Modifier.clip(restShape), false)); seen += background.contentKey
            }
            add(Triple(top, frontMod, backActive)); seen += top.contentKey
            // Skip a ghost whose key is live again: entries share one SaveableStateHolder,
            // and a duplicate contentKey crashes it.
            exiting.forEach { ex ->
                if (!seen.add(ex.entry.contentKey)) return@forEach
                add(Triple(ex.entry, Modifier.graphicsLayer {
                    scaleX = ex.scale
                    scaleY = ex.scale
                    transformOrigin = TransformOrigin(if (ex.edgeRight) 0.12f else 0.88f, ex.touchYFrac)
                    translationX = ex.commit.value * this.size.width
                    clip = true
                    shape = RoundedCornerShape(lerp(deviceRadius.toPx(), pbgCorner.toPx(), ex.cornerProgress))
                }, true))
            }
        }
        val backInteraction = backActive || exiting.isNotEmpty()
        layers.forEachIndexed { i, (entry, mod, exit) ->
            key(entry.contentKey) {
                val isTop = entry.contentKey == topKey
                // Every card paints its own ground, since two are on screen during a push or swipe.
                // After `mod` so the fill lands inside the card's clip.
                Box(
                    Modifier
                        .fillMaxSize()
                        .then(mod)
                        .background(MaterialTheme.colorScheme.background)
                        .onPlaced { if (isTop) placed.value = true },
                ) {
                    CompositionLocalProvider(
                        LocalNavCardExiting provides exit,
                        LocalNavForeground provides (isTop && !exit && !showPush),
                    ) {
                        entry.Content()
                    }
                }
            }
            // Scrim over the revealed screen, under everything moving.
            if (backInteraction && i == 0 && layers.size > 1) {
                Box(Modifier.fillMaxSize().drawBehind {
                    val a = if (backActive) SCRIM_MAX else SCRIM_MAX * (1f - (exiting.lastOrNull()?.commit?.value ?: 1f))
                    drawRect(Color.Black, alpha = a)
                })
            }
        }
    }
}
