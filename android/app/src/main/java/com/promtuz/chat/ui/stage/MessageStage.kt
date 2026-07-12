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

/**
 * Scroll + anchor state for [MessageStage]. [scroll] is px of history above the
 * newest edge (0 = pinned to the live bottom). While [pin]ned, scroll is derived
 * each frame so the pinned row's screen position is invariant — content changes
 * grow away from it instead of shifting it.
 */
@Stable
class MessageStageState {
    /** px scrolled up into history; 0 = at the newest message. */
    var scroll by mutableFloatStateOf(0f)
        internal set

    internal var maxScroll = 0f
    internal var innerViewport = 1f
    internal var pinnedKey: Any? = null
    internal var pinnedBottom = 0f
    internal var stackOf: ((Any) -> Float?)? = null

    val isAtBottom: Boolean get() = pinnedKey == null && scroll < 2f

    /** Freeze [key]'s bottom edge at [bottomPx] (stage-root px) until [unpin]. */
    fun pin(key: Any, bottomPx: Float) {
        pinnedKey = key
        pinnedBottom = bottomPx
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
    val factor = Animatable(initialFactor)
    var measuredH = 0
    var exiting = false

    /**
     * Enter decision deferred to the first measure pass that places this row:
     * in-band additions unfold (visible = worth animating), off-band ones snap
     * (history backfill above the viewport).
     */
    var pendingEnter = false

    /** Row data behind state so an update recomposes exactly this slot. */
    val rowState = mutableStateOf<Any?>(null)

    /**
     * The one content lambda this slot ever gets: subcompose() with a stable
     * lambda skips recomposition entirely on unchanged passes.
     */
    val content: @Composable () -> Unit = {
        rowState.value?.let { holder.render.value(it) }
    }

    /**
     * Morph hand-off: px of room already open when this row entered (the vanished
     * row it replaces). Effective height lerps enterFromPx → measuredH over the
     * enter, so the swap is one continuous bubble instead of a collapse + unfold.
     */
    var enterFromPx = 0

    /** Fold pivot for enter/exit (the bubble's tail corner, per row type). */
    var origin = TransformOrigin(0.5f, 1f)

    /**
     * Reorder glide: when this row's stack position JUMPS (a frontier moving to a
     * new watermark — not the continuous factor/resize tracking), the delta lands
     * here and decays to 0 on the shared clock; drawn as a layer translation.
     */
    val glide = Animatable(0f)
    var lastStack = Float.NaN

    fun effectiveHeight(): Float {
        val f = factor.value
        return if (exiting) measuredH * f else enterFromPx + (measuredH - enterFromPx) * f
    }

    /** Key of the next-newer row at removal time — where the exit stays spliced. */
    var afterKey: Any? = null
}

/** Stack jumps larger than this are reorders (glided); smaller deltas are the
 * continuous factor/resize tracking. ponytail: heuristic ceiling — a very tall
 * row entering could near it; raise if a false glide ever shows. */
private const val GLIDE_JUMP_PX = 140f

/**
 * The chat's placement engine — a bottom-anchored, windowed, animated column that
 * owns every motion the framework list kept to itself (NavStage/AppDropMenu
 * precedent). Newest row is index 0 and sits at the bottom edge.
 *
 * - **One clock**: room for entering/exiting rows opens/closes via a factor on
 *   [ChatMotion]; a row resizing mid-list (its content animates its own size)
 *   moves every neighbor in the same measure pass — sync is structural.
 * - **Exits are first-class**: removed rows stay composed, spliced where they
 *   were, and fold away (scaleY toward the bottom) before release.
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
    followThreshold: Dp = 240.dp,
    /**
     * The key of a row removed in the same emission that this new row visually
     * REPLACES (typing bubble → the message that ended it). The source vanishes
     * without an exit and this row enters from its height — a morph.
     */
    morphFrom: (T) -> Any? = { null },
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
    remember(rows) { holder.diff(rows, key, scope, state, morphFrom, transformOrigin) }

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
        val childConstraints = Constraints(maxWidth = width)

        val display = holder.displayList
        val entities = holder.entities

        // Content-space walk: stack(i) = summed effective heights of rows newer
        // than i. Screen positions need scroll, which (pinned) needs the pinned
        // row's stack — so band with last frame's value and resolve after.
        val provisionalScroll =
            if (state.pinnedKey != null) holder.lastScroll else state.scroll

        val stacks = FloatArray(display.size)
        val placeables = arrayOfNulls<Placeable>(display.size)
        var stack = 0f
        var pinnedStack = -1f

        for (i in display.indices) {
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
            if (e.pendingEnter) {
                e.pendingEnter = false
                holder.scope?.launch {
                    if (inBand) e.factor.animateTo(1f, ChatMotion.spec())
                    else e.factor.snapTo(1f)
                }
            }

            stacks[i] = stack
            if (k == state.pinnedKey) pinnedStack = stack

            // A discontinuous stack jump on a settled, measured row = a reorder
            // (frontier moving to its new watermark): glide from the old spot
            // instead of snapping. Continuous factor/resize tracking stays exempt.
            if (!e.lastStack.isNaN() && !e.exiting && e.measuredH > 0 && e.factor.value >= 1f) {
                val jump = e.lastStack - stack
                if (kotlin.math.abs(jump) > GLIDE_JUMP_PX) {
                    val carried = jump + e.glide.value
                    holder.scope?.launch {
                        e.glide.snapTo(carried)
                        e.glide.animateTo(0f, ChatMotion.spec())
                    }
                }
            }
            e.lastStack = stack

            stack += if (e.measuredH > 0) e.effectiveHeight()
            else ESTIMATED_ROW_PX * e.factor.value
        }

        state.innerViewport = (height - topPad - bottomPad).coerceAtLeast(1).toFloat()
        state.maxScroll = max(0f, stack - state.innerViewport)

        // Resolve scroll: derived while pinned (row's bottom edge invariant),
        // clamped otherwise. Pinned derivation must not read state.scroll or it
        // would self-invalidate every pass.
        val scroll = if (state.pinnedKey != null && pinnedStack >= 0f) {
            val derived = state.pinnedBottom - anchorY + pinnedStack
            state.scroll = derived
            holder.lastScroll = derived
            derived
        } else {
            val clamped = state.scroll.coerceIn(0f, state.maxScroll)
            if (clamped != state.scroll) state.scroll = clamped
            holder.lastScroll = clamped
            clamped
        }

        // scroll > 0 keeps a bottom-pinned (or short) chat from paging on open.
        if (scroll > 0f && state.maxScroll - scroll < state.innerViewport * 1.5f) onNearTop()

        layout(width, height) {
            for (i in display.indices) {
                val p = placeables[i] ?: continue
                val e = entities[holder.keyOf(display[i])] ?: continue
                val bottom = anchorY + scroll - stacks[i]
                val top = bottom - e.measuredH
                if (bottom < -buffer || top > height + buffer) continue
                p.placeWithLayer(0, top.roundToInt()) {
                    // Reorder glide rides the layer, not the layout — animation
                    // frames re-draw without re-measuring.
                    translationY = -e.glide.value
                    val f = e.factor.value
                    if (f < 1f) {
                        // Drawn height matches the room the walk opened (a morph
                        // starts at the vanished row's height, not zero).
                        scaleY = (e.effectiveHeight() / e.measuredH.coerceAtLeast(1)).coerceIn(0f, 1f)
                        scaleX = 0.92f + 0.08f * f
                        alpha = if (e.enterFromPx > 0) 0.5f + 0.5f * f else f
                        this.transformOrigin = e.origin
                    }
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
    var lastScroll = 0f
    var lastWidthPx = 0
    var scope: CoroutineScope? = null
    private val warm = HashMap<Any, SubcomposeLayoutState.PrecomposedSlotHandle>()
    private var lastKeys: List<Any>? = null

    // Snapshot-backed: the measure pass depends on it, and a plain var would leave
    // the layout with no reason to re-run when rows change (first symptom: a chat
    // renders blank until an unrelated invalidation — e.g. the IME — forces a pass).
    private var rows by mutableStateOf<List<Any>>(emptyList())
    private var rawKey: ((Any) -> Any)? = null

    val displayList: List<Any>
        get() {
            if (exiting.isEmpty()) return rows
            val out = ArrayList<Any>(rows.size + exiting.size)
            out.addAll(rows)
            for (e in exiting) {
                val at = e.afterKey?.let { ak -> out.indexOfFirst { keyOf(it) == ak } } ?: -1
                out.add(if (at == -1) 0 else at + 1, e)
            }
            return out
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
            stack += if (e == null || e.measuredH == 0) ESTIMATED_ROW_PX.toFloat() else e.effectiveHeight()
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
    ) {
        rawKey = key as (Any) -> Any
        rows = newRows
        val keys = newRows.map(key)
        val prev = lastKeys
        lastKeys = keys
        val keySet = keys.toHashSet()

        // Morphs claim their source before removals run: the source vanishes with
        // no exit (its room is inherited by the entering row).
        val morphSources = HashMap<Any, Int>()
        for (r in newRows) {
            val src = morphFrom(r) ?: continue
            if (key(r) in entities) continue
            val e = entities[src] ?: continue
            morphSources[src] = e.measuredH
        }

        // Removals → exit in place (spliced after their old newer-neighbor).
        if (prev != null) {
            for ((idx, k) in prev.withIndex()) {
                if (k in keySet) continue
                val e = entities[k] ?: continue
                if (k in morphSources) {
                    exiting.remove(e)
                    entities.remove(k)
                    dropWarm(k)
                    continue
                }
                if (e.exiting) continue
                e.exiting = true
                e.enterFromPx = 0
                e.afterKey = prev.getOrNull(idx - 1)?.takeIf { it in keySet }
                exiting.add(e)
                dropWarm(k)
                scope.launch {
                    e.factor.animateTo(0f, ChatMotion.spec())
                    exiting.remove(e)
                    if (entities[k] === e) entities.remove(k)
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
                e = Entity(k, if (initialFill) 1f else 0f, this)
                e.pendingEnter = !initialFill
                e.enterFromPx = morphFrom(r)?.let { morphSources[it] } ?: 0
                entities[k] = e
            } else if (e.exiting) {
                // Key came back mid-exit (flappy signal): re-enter from where it is.
                e.exiting = false
                exiting.remove(e)
                scope.launch { e.factor.animateTo(1f, ChatMotion.spec()) }
            }
            e.origin = transformOrigin(r)
            if (e.rowState.value != r) e.rowState.value = r
        }

        // A pinned key that vanished entirely releases the pin.
        val pinned = state.pinnedKey
        if (pinned != null && pinned !in keySet && exiting.none { it.key == pinned }) {
            state.unpin()
        }
    }
}
