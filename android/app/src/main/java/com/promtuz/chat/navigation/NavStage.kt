package com.promtuz.chat.navigation

import androidx.activity.BackEventCompat
import androidx.activity.compose.PredictiveBackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.AnimationVector1D
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.tween
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.shape.RoundedCornerShape
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
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/**
 * True for a card that is being animated off (mid back-swipe or sliding off after commit). Read by
 * [com.promtuz.chat.ui.util.freezeOnExit] to freeze live blur before the scale hits it.
 */
val LocalNavCardExiting = compositionLocalOf { false }

private const val FWD_DUR = 260
private const val COMMIT_DUR = 340
private const val CANCEL_DUR = 260
private const val SCALE_TO = 0.90f
private val PBG_CORNER = 24.dp // back-swipe rounds a card up to at least this (flat screens included)
private const val SCRIM_MAX = 0.2f // dim over the revealed screen while back-swiping; lifts on commit
// Cap a single frame's contribution to the push so a janky frame can't jump the slide ahead
// (~2 frames @ 60fps); normal frames fall well under it, only real hitches get clamped.
private const val FRAME_CAP_NANOS = 33_000_000L
private val fwdEase = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)
private val commitEase = CubicBezierEasing(0.3f, 0f, 0.1f, 1f) // EASE_OUT_QUINT-ish fling

/** A card that's been popped and is now a detached ghost sliding off — no longer on the stack. */
private class ExitingCard(
    val entry: NavEntry<NavKey>,
    val scale: Float,
    val edgeRight: Boolean,
    val touchYFrac: Float,
    val cornerProgress: Float,
    val commit: Animatable<Float, AnimationVector1D>,
)

/**
 * A single-stack navigation display, ours end-to-end so we own every pixel of motion — built on
 * nav3's entry primitives ([rememberDecoratedNavEntries] keeps per-entry ViewModelStore + saved
 * state, so a revealed screen keeps its scroll and half-typed text). Replaces nav3's `NavDisplay`.
 *
 * Forward push slides the new screen in from the right over a still previous. Back is the scale-card
 * gesture: the current screen scales toward the finger (drag), and on release the pop is decided
 * *immediately* — the card leaves the stack and becomes a detached [ExitingCard] that slides off on
 * its own, so a follow-up back-swipe already targets the screen beneath instead of fighting the
 * outgoing one. Cancel just springs the scale back. Every card (live + exiting) renders from one
 * keyed loop so a screen changing role is moved by Compose, never disposed (which would wipe state).
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

    val deviceRadius = deviceCornerRadius()         // device's own corner radius; 0 on flat-corner screens
    val pbgCorner = maxOf(deviceRadius, PBG_CORNER) // during back-swipe, round up to at least this
    val restShape = RoundedCornerShape(deviceRadius)
    val size = LocalWindowInfo.current.containerSize
    val widthPx = size.width.toFloat().coerceAtLeast(1f)
    val heightPx = size.height.toFloat().coerceAtLeast(1f)
    val scope = rememberCoroutineScope()

    // Forward (push): new top slides in from the right over a still previous. `forward` is derived in
    // composition (not an effect) so the new screen is already offscreen on the first frame — no flash.
    val topKey = top.contentKey
    var shownKey by remember { mutableStateOf(topKey) }
    var shownSize by remember { mutableIntStateOf(backStack.size) }
    val forward = topKey != shownKey && backStack.size > shownSize
    val enter = remember { Animatable(1f) } // 0 = new fully offscreen right, 1 = settled
    var pushing by remember { mutableStateOf(false) }
    // Fresh per destination (remember(topKey) auto-resets); the incoming card flips it in onPlaced.
    val placed = remember(topKey) { mutableStateOf(false) }
    LaunchedEffect(topKey) {
        val isForward = topKey != shownKey && backStack.size > shownSize
        shownKey = topKey
        shownSize = backStack.size
        if (isForward) {
            pushing = true
            try {
                enter.snapTo(0f)
                // Hold offscreen until the incoming screen has laid out, so its first-frame cost is
                // spent while parked, not eating the slide (timeout is a safety net).
                withTimeoutOrNull(250) { snapshotFlow { placed.value }.first { it } }
                // Drive the slide with a clamped per-frame delta: no single blocked frame can advance
                // it more than FRAME_CAP, so it can't truncate or complete instantly — it plays through.
                val durNanos = FWD_DUR * 1_000_000L
                var last = withFrameNanos { it }
                var elapsed = 0L
                while (elapsed < durNanos) {
                    val now = withFrameNanos { it }
                    elapsed += (now - last).coerceAtMost(FRAME_CAP_NANOS)
                    last = now
                    enter.snapTo(fwdEase.transform((elapsed.toFloat() / durNanos).coerceIn(0f, 1f)))
                }
                enter.snapTo(1f)
            } finally {
                pushing = false
            }
        }
    }
    // enter.value is read ONLY inside the graphicsLayer lambdas below, never in composition — so the
    // push animates by redraw, with no per-frame recomposition of the stage or the screens.
    val showPush = forward || pushing

    // Back gesture on the live top: scale follows the finger; release detaches a ghost, cancel springs.
    var backActive by remember { mutableStateOf(false) }
    var touchY by remember { mutableFloatStateOf(heightPx / 2f) }
    var fromRight by remember { mutableStateOf(false) }
    val progress = remember { Animatable(0f) }
    val exiting = remember { mutableStateListOf<ExitingCard>() }

    PredictiveBackHandler(enabled = entries.size > 1) { events: Flow<BackEventCompat> ->
        backActive = true
        progress.snapTo(0f) // a fresh gesture always takes over an un-scaled live top
        try {
            events.collect { e ->
                touchY = e.touchY
                fromRight = e.swipeEdge == BackEventCompat.EDGE_RIGHT
                progress.snapTo(e.progress)
            }
            // Released past threshold → the pop is decided NOW. Detach the card as a ghost, pop it off
            // the stack immediately, and slide it off in `scope` (survives the next gesture's coroutine).
            val leaving = ExitingCard(
                entry = top,
                scale = lerp(1f, SCALE_TO, progress.value),
                edgeRight = fromRight,
                touchYFrac = (touchY / heightPx).coerceIn(0f, 1f),
                cornerProgress = progress.value,
                commit = Animatable(0f),
            )
            exiting.add(leaving)
            onBack()
            backActive = false
            progress.snapTo(0f)
            scope.launch {
                leaving.commit.animateTo(1f, tween(COMMIT_DUR, easing = commitEase))
                exiting.remove(leaving)
            }
        } catch (c: Throwable) {
            progress.animateTo(0f, tween(CANCEL_DUR, easing = fwdEase)) // cancelled → spring back to rest
            backActive = false
            throw c
        }
    }

    // Live top transform. graphicsLayer lambdas read the animatables (draw-only, no recomposition).
    val frontMod = when {
        backActive -> Modifier.graphicsLayer {
            val s = lerp(1f, SCALE_TO, progress.value)
            scaleX = s
            scaleY = s
            transformOrigin = TransformOrigin(
                if (fromRight) 0.12f else 0.88f,
                (touchY / heightPx).coerceIn(0f, 1f),
            )
            // Round the card up as the swipe progresses, so a flat-corner screen still forms a card.
            clip = true
            shape = RoundedCornerShape(lerp(deviceRadius.toPx(), pbgCorner.toPx(), progress.value))
        }
        showPush -> Modifier
            .graphicsLayer { translationX = (1f - (if (forward) 0f else enter.value)) * widthPx }
            .clip(restShape)
        else -> Modifier.clip(restShape)
    }

    Box(modifier.fillMaxSize()) {
        // Bottom-to-top: revealed background, then the live top, then any detached exiting ghosts.
        // One keyed loop → a screen changing role (top ⇄ revealed) is matched by contentKey and MOVED
        // by Compose, not disposed+recreated (which would reset its scroll/state, and pop a ghost early).
        val layers = buildList {
            if ((backActive || showPush) && below != null) add(Triple(below, Modifier.clip(restShape), false))
            add(Triple(top, frontMod, backActive))
            exiting.forEach { ex ->
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
                Box(Modifier.fillMaxSize().then(mod).onPlaced { if (isTop) placed.value = true }) {
                    CompositionLocalProvider(LocalNavCardExiting provides exit) {
                        entry.Content()
                    }
                }
            }
            // Scrim over the revealed screen (index 0), under everything moving. Full while dragging,
            // fading to nothing as the topmost ghost slides off on commit.
            if (backInteraction && i == 0 && layers.size > 1) {
                Box(Modifier.fillMaxSize().drawBehind {
                    val a = if (backActive) SCRIM_MAX else SCRIM_MAX * (1f - (exiting.lastOrNull()?.commit?.value ?: 1f))
                    drawRect(Color.Black, alpha = a)
                })
            }
        }
    }
}
