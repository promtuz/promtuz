package com.promtuz.chat.ui.stage

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.TweenSpec
import androidx.compose.animation.core.animate
import androidx.compose.animation.core.tween
import androidx.compose.foundation.gestures.Orientation
import androidx.compose.foundation.gestures.rememberScrollableState
import androidx.compose.foundation.gestures.scrollable
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.geometry.Rect
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.Outline
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.layout.Placeable
import androidx.compose.ui.layout.SubcomposeLayout
import androidx.compose.ui.layout.SubcomposeLayoutState
import androidx.compose.ui.layout.SubcomposeSlotReusePolicy
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import kotlin.math.max
import kotlin.math.roundToInt
import kotlinx.coroutines.Job
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

/** Every chat motion runs on this spec so simultaneous movements read as one event. */
object ChatMotion {
    val Easing = CubicBezierEasing(0.19919f, 0.01064f, 0.27921f, 0.91025f)
    const val DURATION_MS = 220
    fun <T> spec(): TweenSpec<T> = tween(DURATION_MS, easing = Easing)
}

/** Bubble geometry is recorded by the renderer; row spacing stays owned by the stage. */
internal class StageBubbleMotion(val progress: () -> Float) {
    var size = Size.Zero
    var dotsPhase = 0f
    var source by mutableStateOf<BubbleSnapshot?>(null)
    var drawsMorph = false
}

internal data class BubbleSnapshot(val size: Size, val dotsPhase: Float, val opacity: Float)
internal val LocalStageBubbleMotion = staticCompositionLocalOf<StageBubbleMotion?> { null }

@Stable
class MessageStageState {
    /** px scrolled up into history; 0 = at the newest message. */
    var scroll by mutableFloatStateOf(0f)
        internal set

    internal var maxScroll = 0f
    internal var innerViewport = 1f
    internal var pinnedKey by mutableStateOf<Any?>(null)
    internal var currentPush = 0
    internal var pinnedPushAtStart = 0
    internal var pinnedBottom = 0f
    /** Actual movement of the pinned row, including viewport clamping. */
    var pinnedOffsetY by mutableFloatStateOf(0f)
        internal set
    internal var stackOf: ((Any) -> Float?)? = null

    /** Hold [key] at [bottomPx] (stage-root px), subject to viewport limits, until [unpin]. */
    fun pin(key: Any, bottomPx: Float) {
        pinnedOffsetY = 0f
        pinnedBottom = bottomPx
        pinnedPushAtStart = currentPush
        pinnedKey = key
    }

    fun unpin() {
        pinnedKey = null
    }

    suspend fun scrollToBottom() {
        if (pinnedKey != null || scroll == 0f) return
        animate(scroll, 0f, animationSpec = ChatMotion.spec()) { v, _ -> scroll = v }
    }

    suspend fun scrollToKey(key: Any) {
        // A history jump can add rows in the caller's composition before Scaffold's
        // subcomposed stage has measured them. Wait for that layout's scroll extent.
        withFrameNanos { }
        if (pinnedKey != null) return
        if (stackOf?.invoke(key) == null) return
        val start = scroll
        animate(0f, 1f, animationSpec = ChatMotion.spec()) { progress, _ ->
            // Rows encountered during a long glide replace estimated heights with
            // real ones. Follow the message's current position as those settle.
            val stack = stackOf?.invoke(key) ?: return@animate
            val target = (stack - innerViewport * 0.4f).coerceIn(0f, maxScroll)
            scroll = start + (target - start) * progress
        }
    }
}

@Composable
fun rememberMessageStageState(): MessageStageState = remember { MessageStageState() }

/** A row's live placement record; heights and enter/exit factors drive the walk. */
private class Entity(val key: Any, initialFactor: Float, holder: StageHolder) {
    /** 0→1 entering (room opens), 1→0 exiting (room closes). */
    var factor = Animatable(initialFactor)
    var sendTransition: SendTransition? = null
    var launchDistance = 0f
    var exitMorphPhase: Float? = null
    val bubbleMotion = StageBubbleMotion { exitMorphPhase ?: factor.value }
    var measuredH = 0
    var exiting = false
    var stickyHeader = false

    /** Resolved by the first measure pass: in-band rows unfold, off-band history backfill snaps. */
    var pendingEnter = false
    var motion: Job? = null

    /** Row data behind state so an update recomposes exactly this slot. */
    val rowState = mutableStateOf<Any?>(null)

    /** The slot's only content lambda, so subcompose() skips unchanged passes. */
    val content: @Composable () -> Unit = {
        CompositionLocalProvider(LocalStageBubbleMotion provides bubbleMotion) {
            rowState.value?.let { holder.render.value(it) }
        }
    }

    /** Room inherited from a morph source; the enter lerps it to measuredH. */
    var enterFromPx = 0
    var exitBaseH = 0f
    var exitTravel = 0f

    /** Fold pivot for enter/exit (the bubble's tail corner, per row type). */
    var origin = TransformOrigin(0.5f, 1f)

    fun effectiveHeight(): Float {
        val f = factor.value
        return if (exiting) exitBaseH * f else enterFromPx + (measuredH - enterFromPx) * f
    }

    fun travel(): Float = when {
        exiting -> exitTravel * factor.value
        enterFromPx > 0 -> 0f
        else -> launchDistance * (1f - factor.value)
    }

    /** Only the part above the live bottom edge displaces older rows. */
    fun occupiedHeight(): Float = (effectiveHeight() - travel()).coerceAtLeast(0f)

    /** Next surviving older row; null keeps the exit at the history edge. */
    var beforeKey: Any? = null
}

/** Bottom-anchored, windowed chat column. Rows are newest first; removed rows exit in place. */
@Composable
fun <T : Any> MessageStage(
    rows: List<T>,
    key: (T) -> Any,
    state: MessageStageState,
    contentPadding: PaddingValues,
    modifier: Modifier = Modifier,
    /**
     * Share of the bottom padding that lifts rows instead of covering them (the reply/edit block).
     * A lambda so its animation is read in measure, not composition.
     */
    pushBottom: () -> Dp = { 0.dp },
    followThreshold: Dp = 240.dp,
    /** False while the initial query is pending; true also for successfully loaded empty history. */
    historyLoaded: Boolean = true,
    /** Key of a row removed in the same emission that this row replaces, as a message replaces typing dots. */
    morphFrom: (T) -> Any? = { null },
    /** Transient rows can enter before or alongside the first history snapshot. */
    animateOnInitialFill: (T) -> Boolean = { false },
    /** An outgoing send can share the exact clock used to clear its composer. */
    entranceClock: (T) -> SendTransition? = { null },
    /** Distance below the composer at birth, captured once for each new row. */
    enterFromBelow: (T) -> Float = { 0f },
    horizontalPivotInset: Dp = 0.dp,
    transformOrigin: (T) -> TransformOrigin = { TransformOrigin(0.5f, 1f) },
    /** Section headers pin below the top inset until the next newer header pushes them away. */
    stickyHeader: (T) -> Boolean = { false },
    /** Fires on every measure pass near the top of loaded history, so it must guard itself. */
    onNearTop: () -> Unit = {},
    row: @Composable (T) -> Unit,
) {
    val scope = rememberCoroutineScope()
    val holder = remember { StageHolder() }
    DisposableEffect(holder, state) {
        onDispose {
            holder.disposeWarm()
            state.stackOf = null
        }
    }
    state.stackOf = holder::stackEstimate
    holder.scope = scope
    // Slots read the renderer through state so each entity keeps one content lambda.
    // Wrapped, not cast: composable function types can't be cast at runtime.
    holder.render.value = { any ->
        @Suppress("UNCHECKED_CAST")
        row(any as T)
    }

    // Diff rows synchronously (before measure) so removals never blink out for a
    // frame; animations launch on the composition scope so they survive re-diffs.
    remember(rows, historyLoaded) {
        val animateArrivals = holder.hasPresentedHistory
        holder.diff(rows, key, scope, state, morphFrom, transformOrigin,
            { animateArrivals || animateOnInitialFill(it) }, entranceClock, enterFromBelow, stickyHeader)
        // Latch after the diff, inside the stage's composition (Scaffold subcomposes it).
        // Loaded empty history matters too: its first future message is a live arrival.
        if (historyLoaded) holder.hasPresentedHistory = true
    }

    val scrollable = rememberScrollableState { delta ->
        if (state.pinnedKey != null) 0f
        else {
            val old = state.scroll
            state.scroll = (old + delta).coerceIn(0f, state.maxScroll)
            state.scroll - old
        }
    }

    val subcomposeState = remember { SubcomposeLayoutState(SubcomposeSlotReusePolicy(12)) }

    // Precompose two cold rows per frame so a first fling doesn't compose them mid-scroll.
    LaunchedEffect(rows) {
        while (holder.lastWidthPx == 0) withFrameNanos { }
        val cold = holder.coldKeys()
        var i = 0
        while (i < cold.size) {
            withFrameNanos { }
            var n = 0
            while (i < cold.size && n < 2) {
                holder.prewarm(cold[i], subcomposeState)
                i++
                n++
            }
        }
    }

    SubcomposeLayout(
        state = subcomposeState,
        modifier = modifier
            .clipToBounds()
            .scrollable(scrollable, Orientation.Vertical),
    ) { constraints ->
        val width = constraints.maxWidth
        val height = constraints.maxHeight
        holder.lastWidthPx = width
        val topPad = contentPadding.calculateTopPadding().roundToPx()
        val bottomPad = contentPadding.calculateBottomPadding().roundToPx()
        val anchorY = height - bottomPad
        val buffer = 400

        // Rows sit at anchorY + scroll - stack, so inset growth lifts them. A scrolled-up view
        // cancels the hold share with scroll += delta; the resolve below clamps it.
        val pushPad = pushBottom().roundToPx()
        val holdPad = bottomPad - pushPad
        if (holder.lastHoldPad >= 0) {
            // Both shares round through Dp independently, so their difference can
            // drift a pixel between frames; a real inset change never does.
            val delta = holdPad - holder.lastHoldPad
            if (kotlin.math.abs(delta) > 1 && state.pinnedKey == null && state.scroll > 2f) {
                state.scroll = max(0f, state.scroll + delta)
            }
        }
        holder.lastHoldPad = holdPad

        // Capture the baseline in pin(), not the first later measure: that measure
        // may already contain the first frame of reply/edit growth.
        state.currentPush = pushPad
        val childConstraints = Constraints(maxWidth = width)

        val display = holder.displayList
        val entities = holder.entities

        // stack(i) sums the heights of rows newer than i. A pinned scroll needs the pinned
        // row's stack, so band with last frame's scroll and resolve after the walk.
        val provisionalScroll =
            if (state.pinnedKey != null) holder.lastScroll else state.scroll

        holder.reserve(display.size)
        val stacks = holder.stacks
        val placeables = holder.placeables
        var stack = 0f
        var pinnedStack = -1f

        for (i in display.indices) {
            placeables[i] = null
            val item = display[i]
            val k = holder.keyOf(item)
            val e = entities.getOrPut(k) { Entity(k, 1f, holder).also { it.rowState.value = item } }

            // Off-band heights walk as estimates; corrections land above the viewport and move only the clamp.
            val bottom = anchorY + provisionalScroll - stack
            val estH = if (e.measuredH > 0) e.measuredH else ESTIMATED_ROW_PX
            val inBand = bottom > -buffer && bottom - estH < height + buffer
            if (inBand) {
                val p = subcompose(k, e.content).first().measure(childConstraints)
                e.measuredH = p.height
                placeables[i] = p
            }
            if (e.pendingEnter && (inBand || e.sendTransition == null)) {
                e.pendingEnter = false
                e.motion?.cancel()
                if (e.sendTransition?.measured() == false) {
                    // The composer fallback already ran while this row was absent.
                    // Its late arrival still gets a complete, independent entrance.
                    e.sendTransition = null
                    e.factor = Animatable(0f)
                }
                if (e.sendTransition == null) e.motion = holder.scope?.launch {
                    if (inBand) e.factor.animateTo(1f, ChatMotion.spec())
                    else e.factor.snapTo(1f)
                }
            }

            if (!e.exiting && e.factor.value >= 1f && e.bubbleMotion.source != null) {
                // A completed handoff is an ordinary message from here on. A later
                // deletion must not run the morph backward and bring dots back.
                e.bubbleMotion.source = null
                e.bubbleMotion.drawsMorph = false
                e.enterFromPx = 0
            }
            stacks[i] = stack
            if (k == state.pinnedKey) pinnedStack = stack

            stack += if (e.measuredH > 0) e.occupiedHeight()
            else ESTIMATED_ROW_PX * e.factor.value
        }

        state.innerViewport = (height - topPad - bottomPad).coerceAtLeast(1).toFloat()
        state.maxScroll = max(0f, stack - state.innerViewport)

        // Derived while pinned, clamped otherwise. The pinned branch must not read
        // state.scroll or it invalidates itself every pass.
        val scroll = if (state.pinnedKey != null && pinnedStack >= 0f) {
            val derived =
                state.pinnedBottom - (pushPad - state.pinnedPushAtStart) - anchorY + pinnedStack
            // A frozen position outside the scroll range can't survive unpinning. Clamping
            // now keeps the lifted copy from jumping when the menu releases the row.
            val clamped = derived.coerceIn(0f, state.maxScroll)
            state.pinnedOffsetY = anchorY + clamped - pinnedStack - state.pinnedBottom
            state.scroll = clamped
            holder.lastScroll = clamped
            clamped
        } else {
            val clamped = state.scroll.coerceIn(0f, state.maxScroll)
            if (clamped != state.scroll) state.scroll = clamped
            holder.lastScroll = clamped
            clamped
        }

        // scroll > 0 keeps a bottom-pinned (or short) chat from paging on open.
        if (scroll > 0f && state.maxScroll - scroll < state.innerViewport * 1.5f) onNearTop()

        var stickyIndex = -1
        var nextHeaderTop = Float.POSITIVE_INFINITY
        for (i in display.indices) {
            val entity = entities[holder.keyOf(display[i])] ?: continue
            if (!entity.stickyHeader || entity.exiting) continue
            val rowHeight = entity.measuredH.takeIf { it > 0 } ?: ESTIMATED_ROW_PX
            val top = anchorY + scroll - stacks[i] - rowHeight
            if (top <= topPad) {
                stickyIndex = i
                break
            }
            nextHeaderTop = top
        }
        // A section can span many off-screen rows. Reuse its header slot without
        // measuring that whole section or changing its estimated scroll extent.
        val sticky = if (stickyIndex >= 0) {
            val entity = entities.getValue(holder.keyOf(display[stickyIndex]))
            placeables[stickyIndex] ?: subcompose(entity.key, entity.content).first().measure(childConstraints)
        } else null
        val stickyY = sticky?.let { minOf(topPad.toFloat(), nextHeaderTop - it.height) } ?: 0f
        val stickyClip = StickyHeaderClip((topPad - stickyY).coerceAtLeast(0f))

        val pivotInset = horizontalPivotInset.toPx()
        layout(width, height) {
            for (i in display.indices) {
                if (i == stickyIndex) continue
                val p = placeables[i] ?: continue
                val e = entities[holder.keyOf(display[i])] ?: continue
                // Pagination can replace a section's header. Keep the old row's
                // collapsing space, but let the live header own the label.
                if (e.stickyHeader && e.exiting) continue
                val bottom = anchorY + scroll - stacks[i]
                val top = bottom - e.measuredH
                if (bottom < -buffer || top > height + buffer) continue
                p.placeWithLayer(0, top.roundToInt()) {
                    val f = e.factor.value
                    val morph = e.bubbleMotion.drawsMorph && e.bubbleMotion.source != null
                    val morphExitScale = if (e.exiting)
                        (f / (e.exitMorphPhase ?: 1f).coerceAtLeast(0.0001f)).coerceIn(0f, 1f) else 1f
                    val scale = if (morph) morphExitScale else
                        (e.effectiveHeight() / e.measuredH.coerceAtLeast(1)).coerceIn(0f, 1f)
                    // A full-width row's edge is not the bubble's, and a moving inset must
                    // not make a bottom pivot read as a top pivot, so place the bounds by hand.
                    val pivotX = pivotInset + (width - 2f * pivotInset) * e.origin.pivotFractionX
                    translationX = pivotX * (1f - scale)
                    translationY = e.measuredH * (1f - scale) + e.travel()
                    scaleX = scale
                    scaleY = scale
                    alpha = if (morph) morphExitScale else if (e.enterFromPx > 0) 1f else f
                    this.transformOrigin = TransformOrigin(0f, 0f)
                }
            }
            sticky?.placeWithLayer(0, stickyY.roundToInt(), zIndex = 1f) {
                clip = true
                shape = stickyClip
            }
        }
    }

    val followPx = with(LocalDensity.current) { followThreshold.toPx() }
    val bottomKey = rows.firstOrNull()?.let(key)
    remember(bottomKey) {
        if (bottomKey != null && state.pinnedKey == null &&
            state.scroll > 2f && state.scroll < followPx
        ) scope.launch { state.scrollToBottom() }
    }
}

private const val ESTIMATED_ROW_PX = 120

private data class StickyHeaderClip(val top: Float) : Shape {
    override fun createOutline(size: Size, layoutDirection: LayoutDirection, density: Density) =
        Outline.Rectangle(Rect(0f, top.coerceAtMost(size.height), size.width, size.height))
}

private class StageHolder {
    val entities = HashMap<Any, Entity>()
    val exiting = mutableStateListOf<Entity>()
    val render = mutableStateOf<@Composable (Any) -> Unit>({})
    var stacks = FloatArray(0)
        private set
    var placeables = arrayOfNulls<Placeable>(0)
        private set

    fun reserve(count: Int) {
        if (stacks.size < count) {
            stacks = FloatArray(maxOf(count, stacks.size * 2, 16))
            placeables = arrayOfNulls(stacks.size)
        }
    }

    var lastScroll = 0f
    var lastWidthPx = 0

    /** Last frame's hold-share of the bottom inset; -1 until the first pass. */
    var lastHoldPad = -1

    var scope: CoroutineScope? = null
    private val warm = HashMap<Any, SubcomposeLayoutState.PrecomposedSlotHandle>()
    private var lastKeys: List<Any>? = null
    var hasPresentedHistory = false

    // Snapshot state: the measure pass reads it, and a plain var leaves the layout
    // no reason to rerun when rows change.
    private var rows by mutableStateOf<List<Any>>(emptyList())
    private var rawKey: ((Any) -> Any)? = null

    val displayList: List<Any> by derivedStateOf {
            if (exiting.isEmpty()) return@derivedStateOf rows
            val out = ArrayList<Any>(rows.size + exiting.size)
            out.addAll(rows)
            for (e in exiting) {
                val at = e.beforeKey?.let { bk -> out.indexOfFirst { keyOf(it) == bk } } ?: -1
                out.add(if (at == -1) out.size else at, e)
            }
            out
        }

    fun keyOf(item: Any): Any = if (item is Entity) item.key else rawKey!!(item)

    fun coldKeys(): List<Any> = displayList.mapNotNull { item ->
        val k = keyOf(item)
        k.takeIf { (entities[k]?.measuredH ?: 0) == 0 && k !in warm }
    }

    /** Compose + measure a cold row off-frame; the walk adopts the slot later. */
    fun prewarm(k: Any, layoutState: SubcomposeLayoutState) {
        val e = entities[k] ?: return
        if (e.measuredH > 0 || k in warm) return
        runCatching {
            val handle = layoutState.precompose(k, e.content)
            handle.premeasure(0, Constraints(maxWidth = lastWidthPx))
            warm[k] = handle
        }
    }

    fun disposeWarm() {
        warm.values.forEach { runCatching { it.dispose() } }
        warm.clear()
    }

    private fun dropWarm(k: Any) {
        warm.remove(k)?.let { runCatching { it.dispose() } }
    }

    fun stackEstimate(key: Any): Float? {
        var stack = 0f
        for (item in displayList) {
            val k = keyOf(item)
            val e = entities[k]
            if (k == key) return stack
            stack += if (e == null || e.measuredH == 0) ESTIMATED_ROW_PX.toFloat() else e.occupiedHeight()
        }
        return null
    }

    @Suppress("UNCHECKED_CAST")
    fun <T : Any> diff(
        newRows: List<T>,
        key: (T) -> Any,
        scope: CoroutineScope,
        state: MessageStageState,
        morphFrom: (T) -> Any?,
        transformOrigin: (T) -> TransformOrigin,
        animateEntrance: (T) -> Boolean,
        entranceClock: (T) -> SendTransition?,
        enterFromBelow: (T) -> Float,
        stickyHeader: (T) -> Boolean,
    ) {
        val previousDisplayKeys = displayList.map(::keyOf)
        rawKey = key as (Any) -> Any
        rows = newRows
        val keys = newRows.map(key)
        val prev = lastKeys
        lastKeys = keys
        val keySet = keys.toHashSet()

        // Morphs claim their source before removals run: the source vanishes with
        // no exit (its room is inherited by the entering row).
        val morphSources = HashMap<Any, Int>()
        val bubbleSources = HashMap<Any, BubbleSnapshot>()
        val claims = HashMap<Any, Any>()
        for (r in newRows) {
            val src = morphFrom(r) ?: continue
            if (key(r) in entities || src in keySet) continue
            val e = entities[src] ?: continue
            if (src in morphSources) continue // One incoming row can claim a typing surface.
            claims[key(r)] = src
            morphSources[src] = e.effectiveHeight().roundToInt()
            val scale = e.factor.value
            if (e.bubbleMotion.size != Size.Zero) {
                bubbleSources[src] = BubbleSnapshot(e.bubbleMotion.size * scale, e.bubbleMotion.dotsPhase, scale)
            }
        }

        for (src in morphSources.keys) {
            entities.remove(src)?.let { e ->
                e.motion?.cancel()
                e.pendingEnter = false
                exiting.remove(e)
                dropWarm(src)
            }
        }

        // Removals keep their place until their exit finishes.
        if (prev != null) {
            for (k in prev) {
                if (k in keySet) continue
                val e = entities[k] ?: continue
                if (e.exiting) continue
                e.motion?.cancel()
                e.pendingEnter = false
                // Exit owns a fresh clock. Reversing the send clock would also
                // expand the composer again and resurrect its captured text.
                if (e.sendTransition != null) {
                    e.factor = Animatable(e.factor.value)
                    e.sendTransition = null
                }
                if (e.bubbleMotion.drawsMorph && e.bubbleMotion.source != null) e.exitMorphPhase = e.factor.value
                e.exitBaseH = if (e.factor.value > 0f) e.effectiveHeight() / e.factor.value else 0f
                e.exitTravel = if (e.factor.value > 0f) e.travel() / e.factor.value else 0f
                e.exiting = true
                exiting.add(e)
                dropWarm(k)
                e.motion = scope.launch {
                    e.factor.animateTo(0f, ChatMotion.spec())
                    exiting.remove(e)
                    if (e.exiting && entities[k] === e) entities.remove(k)
                }
            }
        }

        for (r in newRows) {
            val k = key(r)
            var e = entities[k]
            if (e == null) {
                val animate = animateEntrance(r) || morphFrom(r) in morphSources
                e = Entity(k, if (animate) 0f else 1f, this)
                val sharedClock = entranceClock(r)
                if (sharedClock != null) {
                    e.factor = sharedClock.progress
                    e.sendTransition = sharedClock
                }
                e.launchDistance = enterFromBelow(r).coerceAtLeast(0f)
                e.pendingEnter = animate || sharedClock != null
                e.enterFromPx = claims[k]?.let { morphSources[it] } ?: 0
                e.bubbleMotion.source = claims[k]?.let { bubbleSources[it] }
                entities[k] = e
            } else if (e.exiting) {
                // Key came back mid-exit (flappy signal): re-enter from where it is.
                e.motion?.cancel()
                val f = e.factor.value
                if (f < 1f) e.enterFromPx = ((e.effectiveHeight() - e.measuredH * f) / (1f - f))
                    .coerceAtLeast(0f).roundToInt()
                if (f < 1f) e.launchDistance = e.travel() / (1f - f)
                e.exiting = false
                e.exitMorphPhase = null
                exiting.remove(e)
                e.motion = scope.launch { e.factor.animateTo(1f, ChatMotion.spec()) }
            }
            e.origin = transformOrigin(r)
            e.stickyHeader = stickyHeader(r)
            if (e.rowState.value != r) e.rowState.value = r
        }

        // Another burst may remove an exit's neighbor mid-animation, so re-anchor every exit.
        // Evicted history keeps its old order above the live rows.
        var olderKey: Any? = null
        val orderedExits = ArrayList<Entity>(exiting.size)
        for (k in previousDisplayKeys.asReversed()) {
            if (k in keySet) olderKey = k
            else entities[k]?.takeIf { it.exiting }?.let { e ->
                e.beforeKey = olderKey
                orderedExits.add(e)
            }
        }
        exiting.clear()
        exiting.addAll(orderedExits.asReversed())

        val pinned = state.pinnedKey
        if (pinned != null && pinned !in keySet && exiting.none { it.key == pinned }) {
            state.unpin()
        }
    }
}
