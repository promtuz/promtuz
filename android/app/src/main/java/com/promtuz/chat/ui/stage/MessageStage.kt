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
import androidx.compose.ui.layout.Placeable
import androidx.compose.ui.layout.SubcomposeLayout
import androidx.compose.ui.layout.SubcomposeLayoutState
import androidx.compose.ui.layout.SubcomposeSlotReusePolicy
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlin.math.max
import kotlin.math.roundToInt
import kotlinx.coroutines.Job
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

/**
 * One clock for every chat motion. Placement, unfold, resize, crossfade — all
 * chat animation runs on this spec so simultaneous movements read as one event.
 */
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

/**
 * Scroll + anchor state for [MessageStage]. [scroll] is px of history above the
 * newest edge (0 = pinned to the live bottom). While [pin]ned, scroll is derived
 * each frame to preserve the row's position within the available scroll range.
 * Composer growth can displace it; pinnedOffsetY lets the lifted copy follow.
 */
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

    val isAtBottom: Boolean get() = pinnedKey == null && scroll < 2f

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

    /** Glide until [key]'s row sits in the upper half of the viewport. */
    suspend fun scrollToKey(key: Any) {
        if (pinnedKey != null) return
        val stack = stackOf?.invoke(key) ?: return
        val target = (stack - innerViewport * 0.4f).coerceIn(0f, maxScroll)
        animate(scroll, target, animationSpec = ChatMotion.spec()) { v, _ -> scroll = v }
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

    /**
     * Enter decision deferred to the first measure pass that places this row:
     * in-band additions unfold (visible = worth animating), off-band ones snap
     * (history backfill above the viewport).
     */
    var pendingEnter = false
    var motion: Job? = null

    /** Row data behind state so an update recomposes exactly this slot. */
    val rowState = mutableStateOf<Any?>(null)

    /**
     * The one content lambda this slot ever gets: subcompose() with a stable
     * lambda skips recomposition entirely on unchanged passes.
     */
    val content: @Composable () -> Unit = {
        CompositionLocalProvider(LocalStageBubbleMotion provides bubbleMotion) {
            rowState.value?.let { holder.render.value(it) }
        }
    }

    /**
     * Morph hand-off: px of room already open when this row entered (the vanished
     * row it replaces). Effective height lerps enterFromPx → measuredH over the
     * enter, so the swap is one continuous bubble instead of a collapse + unfold.
     */
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

/**
 * The chat's placement engine — a bottom-anchored, windowed, animated column that
 * owns every motion the framework list kept to itself (NavStage/AppDropMenu
 * precedent). Newest row is index 0 and sits at the bottom edge.
 *
 * - **One clock**: room for entering/exiting rows opens/closes via a factor on
 *   [ChatMotion]; a row resizing mid-list (its content animates its own size)
 *   moves every neighbor in the same measure pass — sync is structural.
 * - **Exits are first-class**: removed rows stay composed, spliced where they
 *   were, and scale toward their bottom corner before release.
 * - **Anchor policy**: at the bottom, content growth is absorbed by the walk;
 *   near-bottom (< [followThreshold]) the view glides home; scrolled-up it
 *   holds; a [MessageStageState.pin]ned row never moves on screen.
 *
 * Narrow contract by design: single column, newest-first rows, stable [key]s.
 */
@Composable
fun <T : Any> MessageStage(
    rows: List<T>,
    key: (T) -> Any,
    state: MessageStageState,
    contentPadding: PaddingValues,
    modifier: Modifier = Modifier,
    /**
     * The share of [contentPadding]'s bottom that DISPLACES content instead of
     * covering it — the composer's reply/edit block. Growth here rides every row
     * up with the composer at any scroll position; growth in the rest of the
     * bottom inset (IME, attach panel, a text field wrapping a line) only holds
     * a scrolled-up view still and opens the freed space below it.
     *
     * A lambda so the read lands in the measure pass: the composer's reveal is an
     * animation, and sampling it in composition would invalidate every row slot
     * on every frame of it.
     */
    pushBottom: () -> Dp = { 0.dp },
    followThreshold: Dp = 240.dp,
    /**
     * The key of a row removed in the same emission that this new row visually
     * REPLACES (typing bubble → the message that ended it). The source vanishes
     * without an exit and this row enters from its height — a morph.
     */
    morphFrom: (T) -> Any? = { null },
    /** Transient rows enter even when history is being painted for the first time. */
    animateOnInitialFill: (T) -> Boolean = { false },
    /** An outgoing send can share the exact clock used to clear its composer. */
    entranceClock: (T) -> SendTransition? = { null },
    /** Distance below the composer at birth, captured once for each new row. */
    enterFromBelow: (T) -> Float = { 0f },
    horizontalPivotInset: Dp = 0.dp,
    /** Fold pivot per row (a bubble's tail corner); bottom-center default. */
    transformOrigin: (T) -> TransformOrigin = { TransformOrigin(0.5f, 1f) },
    /**
     * Fired from the measure walk while the view sits within ~1.5 viewports of
     * the top of loaded history — the pagination doorbell. Fires every pass in
     * the zone, so the callback must carry its own re-entrancy/exhausted guard.
     */
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
    // Slots read the renderer through state, and each entity owns ONE content
    // lambda for its lifetime: a measure pass with unchanged rows recomposes
    // nothing (a fresh lambda per subcompose() call would invalidate every
    // visible row on every scroll/animation frame). Wrapped, not cast: composable
    // function types can't be runtime-cast (ComposableLambdaImpl is not a
    // FunctionN), only the erased row value can.
    holder.render.value = { any ->
        @Suppress("UNCHECKED_CAST")
        row(any as T)
    }

    // Diff rows synchronously (before measure) so removals never blink out for a
    // frame; animations launch on the composition scope so they survive re-diffs.
    remember(rows) { holder.diff(rows, key, scope, state, morphFrom, transformOrigin, animateOnInitialFill, entranceClock, enterFromBelow) }

    val scrollable = rememberScrollableState { delta ->
        if (state.pinnedKey != null) 0f
        else {
            val old = state.scroll
            state.scroll = (old + delta).coerceIn(0f, state.maxScroll)
            state.scroll - old
        }
    }

    // Rows crossing the band edge recycle retired compositions instead of
    // composing from scratch (fresh-compose + dispose per scroll frame is
    // what a lazy walk costs without a pool).
    val subcomposeState = remember { SubcomposeLayoutState(SubcomposeSlotReusePolicy(12)) }

    // Warm-up prefetch: cold rows compose + text-layout mid-fling otherwise (the
    // first fast scroll's 80ms frames). A couple of off-band rows precompose per
    // frame in the idle gaps until the whole history is warm; the walk later
    // adopts them as ordinary (cheap) slots.
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

        // Rows sit at anchorY + scroll - stack, so a growing bottom inset drops
        // anchorY and rides every row up with it. That's what the push share wants;
        // the rest of the inset instead holds a scrolled-up view where it is, and
        // scroll += delta is exactly the cancellation (maxScroll grows by the same
        // delta as innerViewport shrinks, so the room is always there). At the
        // bottom nothing holds — content tracks the composer. Clamping is left to
        // the resolve below, which is where maxScroll is finally known.
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

        // Content-space walk: stack(i) = summed effective heights of rows newer
        // than i. Screen positions need scroll, which (pinned) needs the pinned
        // row's stack — so band with last frame's value and resolve after.
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

            // Lazy for real: off-band rows are never subcomposed — unknown heights
            // walk as estimates and correct when the band reaches them (corrections
            // land above the viewport, which only moves the clamp, not the view).
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

        // Resolve scroll: derived while pinned (row's bottom edge invariant),
        // clamped otherwise. Pinned derivation must not read state.scroll or it
        // would self-invalidate every pass.
        val scroll = if (state.pinnedKey != null && pinnedStack >= 0f) {
            val derived =
                state.pinnedBottom - (pushPad - state.pinnedPushAtStart) - anchorY + pinnedStack
            // A frozen position outside the scroll range cannot survive unpinning.
            // Apply that limit now so composer/IME growth moves the lifted copy
            // continuously, rather than jumping when the menu releases the row.
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

        val pivotInset = horizontalPivotInset.toPx()
        layout(width, height) {
            for (i in display.indices) {
                val p = placeables[i] ?: continue
                val e = entities[holder.keyOf(display[i])] ?: continue
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
                    // Position the transformed bounds explicitly. A full-width row's
                    // right edge is not the bubble's right edge, and moving composer
                    // insets must not turn a bottom pivot into a top-pivot illusion.
                    val pivotX = pivotInset + (width - 2f * pivotInset) * e.origin.pivotFractionX
                    translationX = pivotX * (1f - scale)
                    translationY = e.measuredH * (1f - scale) + e.travel()
                    scaleX = scale
                    scaleY = scale
                    alpha = if (morph) morphExitScale else if (e.enterFromPx > 0) 1f else f
                    this.transformOrigin = TransformOrigin(0f, 0f)
                }
            }
        }
    }

    // Near-bottom follow: a new bottom row glides the view home; farther up it
    // holds (reading history). Own-send force-follow is the caller's call via
    // scrollToBottom().
    val followPx = with(LocalDensity.current) { followThreshold.toPx() }
    val bottomKey = rows.firstOrNull()?.let(key)
    remember(bottomKey) {
        if (bottomKey != null && state.pinnedKey == null &&
            state.scroll > 2f && state.scroll < followPx
        ) scope.launch { state.scrollToBottom() }
    }
}

private const val ESTIMATED_ROW_PX = 120

/** Composition-side bookkeeping: entity map, exit splicing, display list. */
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

    // Snapshot-backed: the measure pass depends on it, and a plain var would leave
    // the layout with no reason to re-run when rows change (first symptom: a chat
    // renders blank until an unrelated invalidation — e.g. the IME — forces a pass).
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

    /** Keys not yet measured, walk order — the prefetch worklist. */
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

    /** Content-space stack of [key] from cached heights (estimates for unmeasured). */
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
        animateOnInitialFill: (T) -> Boolean,
        entranceClock: (T) -> SendTransition?,
        enterFromBelow: (T) -> Float,
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

        // First fill of an empty stage appears in place — the open paints a whole
        // screenful at once; per-row unfolds are for rows arriving after that.
        val initialFill = prev.isNullOrEmpty()
        for (r in newRows) {
            val k = key(r)
            var e = entities[k]
            if (e == null) {
                val animate = !initialFill || animateOnInitialFill(r) || morphFrom(r) in morphSources
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
            if (e.rowState.value != r) e.rowState.value = r
        }

        // Re-anchor every active exit: another burst may remove its neighbor
        // before the animation finishes. Preserve the old display order, with
        // evicted history staying above the live rows instead of at the bottom.
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

        // A pinned key that vanished entirely releases the pin.
        val pinned = state.pinnedKey
        if (pinned != null && pinned !in keySet && exiting.none { it.key == pinned }) {
            state.unpin()
        }
    }
}
